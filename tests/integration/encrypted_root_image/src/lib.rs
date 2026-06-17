//! Single-source-of-truth whole-disk encrypted-root image fixture for the
//! `plans/PI.md` P11 Chunk B-2 root-mount->login QEMU vertical.
//!
//! [`build_image`] assembles a whole disk of the exact shape `tools/mkimage`
//! writes a real installable image from, through the **real** in-tree
//! drivers and encoders so the fixture cannot drift from the system that
//! mounts the disk (`AGENTS.md` §2.2):
//!
//! 1. An **MBR** ([`rustos_partition::mbr::encode`]) describing two
//!    1 MiB-aligned primary partitions.
//! 2. A **FAT32 boot partition** at [`BOOT_LBA`], authored by the real
//!    [`Fat32`] driver, carrying the plaintext `root.unlock`
//!    key-derivation descriptor ([`ROOT_UNLOCK_NAME`]).
//! 3. An **encrypted `RustFS` root partition** at [`ROOT_LBA`], whose
//!    volume key is **derived from [`PASSPHRASE`]** through the descriptor
//!    above (`AGENTS.md` §11), carrying `/System/Security/Users` with the
//!    single [`USERNAME`]/[`PASSWORD`] account — the shared
//!    [`rustos_test_rustfs_image`] users-root volume.
//!
//! The host harness (`tools/xtask`) plants [`build_image`]'s bytes on the
//! test's virtio-blk backing; the freestanding guest tail
//! (`tests/integration/virtio_qemu_support`) drives the production
//! interactive unlock policy
//! (`rustos_kernel::root_mount::unlock_root_disk_interactively`) over that
//! disk: it types [`PASSPHRASE`] at the prompt, the descriptor re-derives
//! the volume key, the root mounts, the database installs, and the planted
//! [`USERNAME`]/[`PASSWORD`] account authenticates.
//!
//! `no_std` + `alloc` so it links into both the host build tool and the
//! freestanding guest tail (it names the passphrase and the planted
//! account both sides verify).

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use rustos_abi::driver::block::{Block, BlockGeometry};
use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeKind};
use rustos_abi::DriverError;
use rustos_drv_fs_fat32::Fat32;
use rustos_drv_fs_rustfs::{
    EntropySource, UnlockDescriptor, VolumeKey, ROOT_UNLOCK_NAME, UNLOCK_DESCRIPTOR_LEN,
    UNLOCK_MIN_ITERATIONS,
};
use rustos_partition::{mbr, Partition, PartitionType};
use rustos_test_rustfs_image as root_image;

/// Logical block (sector) size of the produced image, in bytes. Matches
/// the 512-byte sector QEMU's virtio-blk reports by default and the sector
/// size every in-tree filesystem driver addresses.
pub const SECTOR_BYTES: usize = 512;

/// First sector of the FAT32 boot partition (1 MiB alignment, the
/// universal SD-card convention `tools/mkimage` uses).
pub const BOOT_LBA: u64 = 2048;

/// Sectors in the FAT32 boot partition: 64 MiB. A valid FAT32 volume needs
/// far more clusters than the tiny `root.unlock` descriptor occupies, so
/// the partition is sized for a real format rather than the descriptor's
/// footprint — the same size `tools/mkimage` formats the boot partition at.
pub const FAT_BOOT_SECTORS: u64 = 131_072;

/// First sector of the `RustFS` root partition: directly after the boot
/// partition, which already ends 1 MiB-aligned.
pub const ROOT_LBA: u64 = BOOT_LBA + FAT_BOOT_SECTORS;

/// Sectors in the encrypted `RustFS` root partition — the shared
/// [`rustos_test_rustfs_image`] users-root volume's footprint.
pub const ROOT_SECTORS: u64 = root_image::TOTAL_SECTORS;

/// Total sectors in the assembled whole-disk image.
pub const TOTAL_SECTORS: u64 = ROOT_LBA + ROOT_SECTORS;

/// The passphrase the test "operator" types at the unlock prompt. The root
/// volume's key is derived from it through the on-disk descriptor; the
/// passphrase itself is stored nowhere in the image.
pub const PASSPHRASE: &[u8] = b"unlock-vertical correct horse battery staple";

/// Username of the single account planted on the root volume — the shared
/// [`rustos_test_rustfs_image`] users-root account, so the guest tail's
/// authentication proof names exactly what the volume carries
/// (`AGENTS.md` §2.2).
pub const USERNAME: &str = root_image::USERS_FIXTURE_USERNAME;

/// Password of the planted [`USERNAME`] account.
pub const PASSWORD: &str = root_image::USERS_FIXTURE_PASSWORD;

/// Deterministic stand-in for the platform RNG seam used to provision the
/// unlock descriptor's salt. A fixed sequence keeps the built image
/// **reproducible** (`AGENTS.md` §19.3); it is fixture scaffolding, never a
/// production entropy source.
struct FixtureEntropy {
    next: u8,
}

impl EntropySource for FixtureEntropy {
    fn fill(&mut self, out: &mut [u8]) -> Result<(), DriverError> {
        for byte in out.iter_mut() {
            *byte = self.next;
            self.next = self.next.wrapping_add(1);
        }
        Ok(())
    }
}

/// In-memory whole-disk / partition [`Block`] double addressing
/// [`SECTOR_BYTES`]-byte sectors, with its geometry derived from its actual
/// backing length (so a 64 MiB FAT partition formats correctly, unlike the
/// fixed-geometry rustfs fixture double).
struct MemDisk {
    store: Vec<u8>,
}

impl MemDisk {
    fn new(sectors: u64) -> Self {
        let len = usize::try_from(sectors).unwrap_or(0) * SECTOR_BYTES;
        Self {
            store: vec![0u8; len],
        }
    }

    fn span(&self, lba: u64, len: usize) -> Result<(usize, usize), DriverError> {
        if len == 0 || len % SECTOR_BYTES != 0 {
            return Err(DriverError::BufferTooSmall);
        }
        let start = usize::try_from(lba)
            .ok()
            .and_then(|l| l.checked_mul(SECTOR_BYTES))
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

impl Block for MemDisk {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: u32::try_from(SECTOR_BYTES).unwrap_or(0),
            block_count: u64::try_from(self.store.len() / SECTOR_BYTES).unwrap_or(0),
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

/// Provision the root volume's `root.unlock` descriptor and the volume key
/// it derives from [`PASSPHRASE`].
///
/// The descriptor carries [`UNLOCK_MIN_ITERATIONS`] — the format floor — so
/// the per-build and per-boot PBKDF2 derivations stay fast under QEMU TCG
/// while still exercising the real key-derivation path (`AGENTS.md` §5.4).
///
/// # Errors
///
/// Propagates any [`DriverError`] from descriptor provisioning or encoding.
fn provision() -> Result<([u8; UNLOCK_DESCRIPTOR_LEN], VolumeKey), DriverError> {
    let descriptor =
        UnlockDescriptor::provision(UNLOCK_MIN_ITERATIONS, &mut FixtureEntropy { next: 7 })?;
    let key = descriptor.derive_volume_key(PASSPHRASE);
    let mut bytes = [0u8; UNLOCK_DESCRIPTOR_LEN];
    descriptor.encode(&mut bytes)?;
    Ok((bytes, key))
}

/// Author the FAT32 boot partition through the real [`Fat32`] driver and
/// plant `descriptor` under [`ROOT_UNLOCK_NAME`] — the exact write
/// `tools/mkimage` performs (`AGENTS.md` §2.2). Returns the partition's
/// on-disk bytes.
fn build_boot_partition(descriptor: &[u8]) -> Result<Vec<u8>, DriverError> {
    let mut fs = Fat32::format(MemDisk::new(FAT_BOOT_SECTORS))?;
    let root = fs.root();
    fs.create(root, ROOT_UNLOCK_NAME.as_bytes(), NodeKind::RegularFile)?;
    let written = fs.write_at(root, ROOT_UNLOCK_NAME.as_bytes(), 0, descriptor)?;
    if written != descriptor.len() {
        return Err(DriverError::DeviceFault);
    }
    fs.flush()?;
    Ok(fs.into_block().store)
}

/// Build the whole-disk encrypted-root image described in the module docs.
///
/// # Errors
///
/// Propagates any [`DriverError`] from descriptor provisioning, FAT/`RustFS`
/// authoring, or the MBR encode. The fixed geometry makes a failure a
/// programming error in this fixture, but it is surfaced rather than
/// panicked so the builder holds to `AGENTS.md` §2.9 in every path it links
/// into.
pub fn build_image() -> Result<Vec<u8>, DriverError> {
    let (descriptor, key) = provision()?;
    let boot = build_boot_partition(&descriptor)?;
    let root = root_image::build_users_root_image_with_key(&key)?;

    let table = mbr::encode(&[
        Partition {
            ty: PartitionType::FatBoot,
            start_lba: BOOT_LBA,
            block_count: FAT_BOOT_SECTORS,
        },
        Partition {
            ty: PartitionType::RustFsRoot,
            start_lba: ROOT_LBA,
            block_count: ROOT_SECTORS,
        },
    ])
    .map_err(|_| DriverError::DeviceFault)?;

    let mut image = vec![0u8; usize::try_from(TOTAL_SECTORS).unwrap_or(0) * SECTOR_BYTES];
    image[..table.len()].copy_from_slice(&table);
    let boot_at = usize::try_from(BOOT_LBA).unwrap_or(0) * SECTOR_BYTES;
    image[boot_at..boot_at + boot.len()].copy_from_slice(&boot);
    let root_at = usize::try_from(ROOT_LBA).unwrap_or(0) * SECTOR_BYTES;
    image[root_at..root_at + root.len()].copy_from_slice(&root);
    Ok(image)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use rustos_partition::{parse_partition_table, PartitionBlock};

    /// The assembled image carries exactly the two partitions, of the right
    /// types, at the documented 1 MiB-aligned offsets.
    #[test]
    fn the_image_carries_the_documented_partition_layout() {
        let bytes = build_image().expect("the whole-disk image assembles");
        assert_eq!(
            bytes.len(),
            usize::try_from(TOTAL_SECTORS).unwrap() * SECTOR_BYTES,
            "the image is exactly the advertised size"
        );
        let mut disk = MemDisk { store: bytes };
        let table = parse_partition_table(&mut disk).expect("the MBR parses");

        let boot = table
            .first_of_type(PartitionType::FatBoot)
            .expect("a FAT boot partition is present");
        assert_eq!(boot.start_lba, BOOT_LBA);
        assert_eq!(boot.block_count, FAT_BOOT_SECTORS);

        let root = table
            .first_of_type(PartitionType::RustFsRoot)
            .expect("a RustFS root partition is present");
        assert_eq!(root.start_lba, ROOT_LBA);
        assert_eq!(root.block_count, ROOT_SECTORS);
    }

    /// The encrypted root window mounts only under the key the on-disk
    /// descriptor derives from [`PASSPHRASE`] — proving the descriptor and
    /// the volume the fixture provisions agree (`AGENTS.md` §2.2 / §5.4).
    #[test]
    fn the_root_window_mounts_under_the_passphrase_derived_key() {
        use rustos_drv_fs_rustfs::RustFs;

        let bytes = build_image().expect("the whole-disk image assembles");
        let mut disk = MemDisk { store: bytes };
        let table = parse_partition_table(&mut disk).expect("the MBR parses");
        let root = table
            .first_of_type(PartitionType::RustFsRoot)
            .expect("a RustFS root partition is present");

        let (_descriptor, key) = provision().expect("the descriptor provisions");
        let window = PartitionBlock::new(disk, root.start_lba, root.block_count)
            .expect("the root window is in range");
        RustFs::open(window, &key).expect("the root mounts under the descriptor-derived key");
    }
}
