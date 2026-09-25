//! Single-source-of-truth whole-disk encrypted-root image fixture for the
//! `plans/PI.md` P11 Chunk B-2 root-mount->login QEMU vertical.
//!
//! [`build_image`] assembles a whole disk of the exact shape `tools/mkimage`
//! writes a real installable image from, through the **real** in-tree
//! drivers and encoders so the fixture cannot drift from the system that
//! mounts the disk:
//!
//! 1. An **MBR** describing three 1 MiB-aligned primary partitions, back to
//!    back, laid out by the one assembly `tools/mkimage` uses
//!    ([`tairix_syshelp::assemble_disk`]).
//! 2. A **FAT32 boot partition** at [`BOOT_PART_LBA`], authored by the real
//!    [`Fat32`] driver, carrying the plaintext `root.unlock`
//!    key-derivation descriptor ([`ROOT_UNLOCK_NAME`]).
//! 3. The read-only **`/System` partition** at [`SYSTEM_PART_LBA`], sized to
//!    the signed bundles and discovered system payload it carries.
//! 4. An **encrypted `ARXFS` root partition** directly after it, whose
//!    volume key is **derived from [`PASSPHRASE`]** through the descriptor
//!    above, carrying `/System/Security/Users` with the
//!    single [`USERNAME`]/[`PASSWORD`] account — the shared
//!    [`tairix_test_arxfs_image`] users-root volume.
//!
//! The host harness (`tools/xtask`) plants [`build_image`]'s bytes on the
//! test's virtio-blk backing; the freestanding guest tail
//! (`tests/integration/virtio_qemu_support`) drives the production
//! interactive unlock policy
//! (`tairix_kernel::root_mount::unlock_root_disk_interactively`) over that
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

use tairix_abi::driver::block::{Block, BlockGeometry};
use tairix_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeId, NodeKind};
use tairix_abi::DriverError;
use tairix_drv_fs_arxfs::{
    EntropySource, UnlockDescriptor, VolumeKey, ARXFS, ROOT_UNLOCK_NAME, SYSTEM_VOLUME_KEY,
    UNLOCK_DESCRIPTOR_LEN, UNLOCK_MIN_ITERATIONS,
};
use tairix_drv_fs_fat32::Fat32;
use tairix_syshelp::DiskLayoutError;
use tairix_test_arxfs_image as root_image;

pub use tairix_syshelp::{BOOT_PART_LBA, BOOT_PART_SECTORS, SECTOR_BYTES, SYSTEM_PART_LBA};

/// Sectors in the encrypted `ARXFS` root partition — the shared
/// [`tairix_test_arxfs_image`] users-root volume's footprint.
pub const ROOT_SECTORS: u64 = root_image::TOTAL_SECTORS;

/// The passphrase the test "operator" types at the unlock prompt. The root
/// volume's key is derived from it through the on-disk descriptor; the
/// passphrase itself is stored nowhere in the image.
pub const PASSPHRASE: &[u8] = b"unlock-vertical correct horse battery staple";

/// Username of the single account planted on the root volume — the shared
/// [`tairix_test_arxfs_image`] users-root account, so the guest tail's
/// authentication proof names exactly what the volume carries.
pub const USERNAME: &str = root_image::USERS_FIXTURE_USERNAME;

/// Password of the planted [`USERNAME`] account.
pub const PASSWORD: &str = root_image::USERS_FIXTURE_PASSWORD;

/// Deterministic stand-in for the platform RNG seam used to provision the
/// unlock descriptor's salt. A fixed sequence keeps the built image
/// **reproducible**; it is fixture scaffolding, never a
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
/// fixed-geometry arxfs fixture double).
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
        if len == 0 || !len.is_multiple_of(SECTOR_BYTES) {
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

    fn flush(&mut self) -> Result<(), DriverError> {
        Ok(())
    }
}

/// Provision the root volume's `root.unlock` descriptor and the volume key
/// it derives from `passphrase`.
///
/// The descriptor carries [`UNLOCK_MIN_ITERATIONS`] — the format floor — so
/// the per-build and per-boot PBKDF2 derivations stay fast under QEMU TCG
/// while still exercising the real key-derivation path.
/// Passing a **blank** `passphrase` builds the installer-profile image,
/// which the bootstrap unlocks with no prompt.
///
/// # Errors
///
/// Propagates any [`DriverError`] from descriptor provisioning or encoding.
fn provision(passphrase: &[u8]) -> Result<([u8; UNLOCK_DESCRIPTOR_LEN], VolumeKey), DriverError> {
    let descriptor =
        UnlockDescriptor::provision(UNLOCK_MIN_ITERATIONS, &mut FixtureEntropy { next: 7 })?;
    let key = descriptor.derive_volume_key(passphrase);
    let mut bytes = [0u8; UNLOCK_DESCRIPTOR_LEN];
    descriptor.encode(&mut bytes)?;
    Ok((bytes, key))
}

/// Author the FAT32 boot partition through the real [`Fat32`] driver and
/// plant `descriptor` under [`ROOT_UNLOCK_NAME`] — the exact write
/// `tools/mkimage` performs. Returns the partition's
/// on-disk bytes.
fn build_boot_partition(descriptor: &[u8]) -> Result<Vec<u8>, DriverError> {
    // A fixed test serial: the fixture's boot partition never meets the
    // volume forest.
    let mut fs = Fat32::format(MemDisk::new(BOOT_PART_SECTORS), 0x0B00_7F1E)?;
    let root = fs.root();
    fs.create(root, ROOT_UNLOCK_NAME.as_bytes(), NodeKind::RegularFile)?;
    let written = fs.write_at(root, ROOT_UNLOCK_NAME.as_bytes(), 0, descriptor)?;
    if written != descriptor.len() {
        return Err(DriverError::DeviceFault);
    }
    fs.flush()?;
    Ok(fs.into_block().store)
}

/// Author the read-only `/System` partition through the sizing and file walk
/// `tools/mkimage` uses (`tairix_syshelp::build_system_volume`): an `ARXFS`
/// volume under the non-secret well-known [`SYSTEM_VOLUME_KEY`] with the
/// `Drivers` and `Security` skeleton at its root, carrying the discovered
/// system payload, `drivers` and `apps`. The kernel mounts it read-only and
/// autoloads the `Drivers/` store **before** unlocking the encrypted root
/// (`plans/PI.md` design B / B2).
///
/// Each file is `(path_components, bytes)`, its path relative to the volume
/// root, which *is* `/System` (e.g. `&[b"Drivers", b"input", b"virtio_kbd",
/// b"Run"]` or `&[b"Commands", b"ls.app", b"Run"]`). The caller composes and
/// signs the files; this fixture only plants them.
fn build_system_partition(
    drivers: &[(&[&[u8]], &[u8])],
    apps: &[(&[&[u8]], &[u8])],
) -> Result<Vec<u8>, DriverError> {
    tairix_syshelp::build_system_volume(&[drivers, apps], |sectors| {
        let mut fs = ARXFS::format(
            MemDisk::new(sectors),
            64,
            &SYSTEM_VOLUME_KEY,
            &mut FixtureEntropy { next: 3 },
        )?;
        let root = fs.root();
        let security = fs.create(root, b"Security", NodeKind::Directory)?;
        fs.create(security, b"Keys", NodeKind::Directory)?;
        fs.create(security, b"Policy", NodeKind::Directory)?;
        fs.create(root, b"Drivers", NodeKind::Directory)?;
        Ok(SystemVolume { fs, root })
    })
}

/// The `/System` volume [`build_system_partition`] is filling.
struct SystemVolume {
    fs: ARXFS<MemDisk>,
    root: NodeId,
}

impl tairix_syshelp::SystemVolume for SystemVolume {
    type Error = DriverError;
    type Image = Vec<u8>;

    fn is_no_space(error: &DriverError) -> bool {
        matches!(error, DriverError::NoSpace)
    }

    fn plant(&mut self, components: &[&[u8]], bytes: &[u8]) -> Result<(), DriverError> {
        root_image::plant_nested_file(&mut self.fs, self.root, components, bytes)
    }

    fn finish(mut self) -> Result<Vec<u8>, DriverError> {
        self.fs.flush()?;
        Ok(self.fs.into_block()?.store)
    }
}

/// Build the whole-disk encrypted-root image described in the module docs.
///
/// # Errors
///
/// Propagates any [`DriverError`] from descriptor provisioning, FAT/`ARXFS`
/// authoring, or the disk assembly: a fixture defect, surfaced rather than
/// panicked because the fixture also links into the freestanding guest.
pub fn build_image() -> Result<Vec<u8>, DriverError> {
    build_image_with_contents(&[], &[], &[], PASSPHRASE)
}

/// Build the whole-disk encrypted-root image with the signed application
/// bundles `apps` planted into the read-only `/System` app/service stores
/// beside their `Help/` trees — each `(path_components, bytes)` a
/// volume-relative bundle file (e.g. `&[b"Apps", b"ls.app", b"AppInfo"]`),
/// composed and signed by the caller (`plans/APPS.md` deliverable 8) —
/// plus `root_files` planted on the **encrypted root volume** (each
/// relative to that volume's root, e.g. the seeded program-library catalog
/// under `System/Settings/…`, which the writable `/System/Settings` child
/// mount rebases onto — `plans/NEW-TASKBAR.md` T3).
///
/// # Errors
///
/// Propagates any [`DriverError`] from the underlying whole-disk assembly
/// (surfaced rather than panicked).
pub fn build_image_with_apps(
    apps: &[(&[&[u8]], &[u8])],
    root_files: &[(&[&[u8]], &[u8])],
) -> Result<Vec<u8>, DriverError> {
    build_image_with_contents(&[], apps, root_files, PASSPHRASE)
}

/// Build the whole-disk encrypted-root image whose root is encrypted under
/// `passphrase` (rather than the default [`PASSPHRASE`]).
///
/// A **blank** `passphrase` builds the installer-profile image: the
/// bootstrap unlocks it with no prompt, so this is the
/// fixture the kernel's silent auto-unlock regression mounts. Delegates to
/// [`build_image_with_contents`] with empty stores (one authoring path).
///
/// # Errors
///
/// Propagates any [`DriverError`] from descriptor provisioning, FAT/`ARXFS`
/// authoring, or the disk assembly (surfaced rather than panicked).
pub fn build_image_with_passphrase(passphrase: &[u8]) -> Result<Vec<u8>, DriverError> {
    build_image_with_contents(&[], &[], &[], passphrase)
}

/// Build the whole-disk encrypted-root image, additionally planting a set of
/// installed driver bundles into the read-only `/System` volume's `Drivers/`
/// store.
///
/// This is [`build_image`] with the discovered driver store populated:
/// each `(path_components, bytes)` is laid into the **read-only `/System`
/// volume**'s `Drivers/` store exactly as a real installation carries it, so
/// the pre-unlock autoload
/// (`tairix_kernel::root_mount::autoload_system_drivers` →
/// `driver_autoload::autoload_from_mounted_root`) discovers, verifies, and
/// spawns the bundle off the `/System` volume *before* the encrypted root is
/// unlocked (design B — the store lives on a volume reachable before the
/// passphrase). [`build_image`] delegates here with no drivers (one authoring path).
///
/// # Errors
///
/// Propagates any [`DriverError`] from descriptor provisioning, FAT/`ARXFS`
/// authoring, or the disk assembly (surfaced rather than panicked).
pub fn build_image_with_drivers(drivers: &[(&[&[u8]], &[u8])]) -> Result<Vec<u8>, DriverError> {
    build_image_with_contents(drivers, &[], &[], PASSPHRASE)
}

/// Build the whole-disk encrypted-root image, planting `drivers` and the
/// application-bundle files `apps` into the read-only `/System` stores,
/// `root_files` onto the **encrypted root volume** (volume-relative — the
/// home of state the writable `/System/Settings` child mount rebases onto,
/// e.g. the seeded program-library catalog), and
/// encrypting the root under `passphrase`.
///
/// The single authoring path behind [`build_image`],
/// [`build_image_with_drivers`], [`build_image_with_apps`], and
/// [`build_image_with_passphrase`]: they differ only in the planted
/// contents and the passphrase the root volume key is derived from.
///
/// # Errors
///
/// Propagates any [`DriverError`] from descriptor provisioning, FAT/`ARXFS`
/// authoring, or the disk assembly (surfaced rather than panicked).
pub fn build_image_with_contents(
    drivers: &[(&[&[u8]], &[u8])],
    apps: &[(&[&[u8]], &[u8])],
    root_files: &[(&[&[u8]], &[u8])],
    passphrase: &[u8],
) -> Result<Vec<u8>, DriverError> {
    let (descriptor, key) = provision(passphrase)?;
    let boot = build_boot_partition(&descriptor)?;
    let system = build_system_partition(drivers, apps)?;
    let root = root_image::build_users_root_image_with_key(&key, root_files)?;
    tairix_syshelp::assemble_disk(&boot, &system, &root).map_err(|refused| match refused {
        DiskLayoutError::Table(_) => DriverError::DeviceFault,
        DiskLayoutError::PartialSector(_)
        | DiskLayoutError::BootLength
        | DiskLayoutError::OutOfRange(_) => DriverError::LengthOutOfRange,
    })
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use tairix_partition::{parse_partition_table, PartitionBlock, PartitionType};

    /// The assembled image carries exactly the three design-B partitions, of
    /// the right types, packed back to back from the documented 1 MiB-aligned
    /// boot offset, and it is exactly as long as the table it carries says.
    ///
    /// `/System` sizes itself to its content, so its length is asserted
    /// against the image's own table and the sizing policy's shape: an exact
    /// byte count would fail on every change to the shipped payload.
    #[test]
    fn the_image_carries_the_documented_partition_layout() {
        let bytes = build_image().expect("the whole-disk image assembles");
        let sectors = u64::try_from(bytes.len() / SECTOR_BYTES).expect("a sane image length");
        assert!(
            bytes.len().is_multiple_of(SECTOR_BYTES),
            "the image is a whole number of sectors"
        );
        let mut disk = MemDisk { store: bytes };
        let table = parse_partition_table(&mut disk).expect("the MBR parses");

        let boot = table
            .first_of_type(PartitionType::FatBoot)
            .expect("a FAT boot partition is present");
        assert_eq!(boot.start_lba, BOOT_PART_LBA);
        assert_eq!(boot.block_count, BOOT_PART_SECTORS);

        let system = table
            .first_of_type(PartitionType::ARXFSSystem)
            .expect("a read-only /System partition is present");
        assert_eq!(
            system.start_lba, SYSTEM_PART_LBA,
            "/System follows the boot"
        );
        let grain = tairix_syshelp::SYSTEM_VOLUME_GRAIN_BYTES / SECTOR_BYTES as u64;
        assert!(
            system.block_count.is_multiple_of(grain),
            "/System is whole grains, keeping the root 1 MiB-aligned: {} sectors",
            system.block_count
        );

        let root = table
            .first_of_type(PartitionType::ARXFSRoot)
            .expect("a ARXFS root partition is present");
        assert_eq!(
            root.start_lba,
            SYSTEM_PART_LBA + system.block_count,
            "the root follows /System with no gap"
        );
        assert_eq!(root.block_count, ROOT_SECTORS);
        assert_eq!(
            sectors,
            root.start_lba + root.block_count,
            "the image is exactly as long as its own table describes"
        );
    }

    /// A planted application-bundle file (`Commands/<name>.app/AppInfo` /
    /// `Run`) reads back byte-for-byte beside the bundle's discovered
    /// `Help/` tree, so every on-disk bundle the verticals browse is
    /// complete and self-contained — and a planted **root-volume** file
    /// (the seeded program-library catalog's home under
    /// `System/Settings/…`, which the writable `/System/Settings` child
    /// mount rebases onto) reads back off the encrypted root.
    #[test]
    fn planted_app_bundle_files_read_back_beside_the_help_tree() {
        const APPINFO: &[u8] = b"a signed AppInfo manifest's bytes (synthetic)";
        const RUN: &[u8] = b"a Run rxe image's bytes (synthetic)";
        const CATALOG: &[u8] =
            b"os.tairix.ls.name ls\nos.tairix.ls.bundle /System/Commands/ls.app\n";
        // Larger than every fixture above, so a read that returned more
        // bytes than were planted would fail the comparison rather than be
        // silently clipped by the buffer.
        const READ_LEN: usize = 128;
        let apps: [(&[&[u8]], &[u8]); 4] = [
            (&[b"Commands", b"ls.app", b"AppInfo"], APPINFO),
            (&[b"Commands", b"ls.app", b"Run"], RUN),
            (&[b"Services", b"login.app", b"AppInfo"], APPINFO),
            (&[b"Services", b"login.app", b"Run"], RUN),
        ];
        let root_files: [(&[&[u8]], &[u8]); 1] = [(
            &[b"System", b"Settings", b"ProgramLibrary", b"library.conf"],
            CATALOG,
        )];
        let bytes =
            build_image_with_apps(&apps, &root_files).expect("the whole-disk image assembles");
        let mut disk = MemDisk {
            store: bytes.clone(),
        };
        let table = parse_partition_table(&mut disk).expect("the MBR parses");
        let system = table
            .first_of_type(PartitionType::ARXFSSystem)
            .expect("a /System partition is present");
        let window = PartitionBlock::new(disk, system.start_lba, system.block_count)
            .expect("the /System window is in range");
        let mut sys = ARXFS::open_read_only(window, &SYSTEM_VOLUME_KEY)
            .expect("the /System volume mounts read-only under the public key");

        for (components, expected) in &apps {
            let mut node = sys.root();
            for component in *components {
                node = sys.lookup(node, component).expect("store path component");
            }
            let mut buf = [0u8; READ_LEN];
            let read = sys.read_at(node, 0, &mut buf).expect("file reads back");
            assert_eq!(&buf[..read], *expected);
        }
        // The command app's bundle also carries its discovered Help/ tree.
        let mut help = sys.root();
        for component in [b"Commands".as_slice(), b"ls.app", b"Help", b"en-US"] {
            help = sys.lookup(help, component).expect("Help path component");
        }
        sys.lookup(help, b"ls.md")
            .expect("the bundle's default help document is planted beside Run");

        // The root-volume file lands on the encrypted root, under the
        // passphrase-derived key, with its intermediate directories.
        let mut disk = MemDisk { store: bytes };
        let table = parse_partition_table(&mut disk).expect("the MBR parses");
        let root_part = table
            .first_of_type(PartitionType::ARXFSRoot)
            .expect("a ARXFS root partition is present");
        let (_descriptor, key) = provision(PASSPHRASE).expect("the descriptor provisions");
        let window = PartitionBlock::new(disk, root_part.start_lba, root_part.block_count)
            .expect("the root window is in range");
        let mut rootvol = tairix_drv_fs_arxfs::ARXFS::open(window, &key)
            .expect("the root mounts under the descriptor-derived key");
        let mut node = rootvol.root();
        for component in root_files[0].0 {
            node = rootvol
                .lookup(node, component)
                .expect("root-volume path component");
        }
        let mut buf = [0u8; READ_LEN];
        let read = rootvol
            .read_at(node, 0, &mut buf)
            .expect("the catalog reads back off the root volume");
        assert_eq!(&buf[..read], CATALOG);
    }

    /// The `/System` partition mounts read-only under the non-secret
    /// well-known key and carries the `Drivers` store directory.
    #[test]
    fn the_system_window_mounts_read_only_under_the_public_key() {
        let bytes = build_image().expect("the whole-disk image assembles");
        let mut disk = MemDisk { store: bytes };
        let table = parse_partition_table(&mut disk).expect("the MBR parses");
        let system = table
            .first_of_type(PartitionType::ARXFSSystem)
            .expect("a /System partition is present");
        let window = PartitionBlock::new(disk, system.start_lba, system.block_count)
            .expect("the /System window is in range");
        let mut sys = ARXFS::open_read_only(window, &SYSTEM_VOLUME_KEY)
            .expect("the /System volume mounts read-only under the public key");
        let root = sys.root();
        sys.lookup(root, b"Drivers")
            .expect("/System/Drivers exists");
    }

    /// The encrypted root window mounts only under the key the on-disk
    /// descriptor derives from [`PASSPHRASE`] — proving the descriptor and
    /// the volume the fixture provisions agree.
    #[test]
    fn the_root_window_mounts_under_the_passphrase_derived_key() {
        use tairix_drv_fs_arxfs::ARXFS;

        let bytes = build_image().expect("the whole-disk image assembles");
        let mut disk = MemDisk { store: bytes };
        let table = parse_partition_table(&mut disk).expect("the MBR parses");
        let root = table
            .first_of_type(PartitionType::ARXFSRoot)
            .expect("a ARXFS root partition is present");

        let (_descriptor, key) = provision(PASSPHRASE).expect("the descriptor provisions");
        let window = PartitionBlock::new(disk, root.start_lba, root.block_count)
            .expect("the root window is in range");
        ARXFS::open(window, &key).expect("the root mounts under the descriptor-derived key");
    }
}
