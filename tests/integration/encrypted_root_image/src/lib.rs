//! Single-source-of-truth whole-disk encrypted-root image fixture for the
//! `plans/PI.md` P11 Chunk B-2 root-mount->login QEMU vertical.
//!
//! [`build_image`] assembles a whole disk of the exact shape `tools/mkimage`
//! writes a real installable image from, through the **real** in-tree
//! drivers and encoders so the fixture cannot drift from the system that
//! mounts the disk:
//!
//! 1. An **MBR** ([`rustos_partition::mbr::encode`]) describing two
//!    1 MiB-aligned primary partitions.
//! 2. A **FAT32 boot partition** at [`BOOT_LBA`], authored by the real
//!    [`Fat32`] driver, carrying the plaintext `root.unlock`
//!    key-derivation descriptor ([`ROOT_UNLOCK_NAME`]).
//! 3. An **encrypted `RustFS` root partition** at [`ROOT_LBA`], whose
//!    volume key is **derived from [`PASSPHRASE`]** through the descriptor
//!    above, carrying `/System/Security/Users` with the
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
    EntropySource, RustFs, UnlockDescriptor, VolumeKey, ROOT_UNLOCK_NAME, SYSTEM_VOLUME_KEY,
    UNLOCK_DESCRIPTOR_LEN, UNLOCK_MIN_ITERATIONS,
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

/// First sector of the read-only `RustFS` `/System` partition: directly
/// after the boot partition, which already ends 1 MiB-aligned. This is the
/// design-B pre-unlock signed-driver store (`plans/PI.md` B1).
pub const SYSTEM_LBA: u64 = BOOT_LBA + FAT_BOOT_SECTORS;

/// Sectors in the read-only `RustFS` `/System` partition: 32 MiB. Large
/// enough to carry the skeleton, the design-B signed driver bundle(s) the
/// pre-unlock autoload reads from its `Drivers/` store (`plans/PI.md`
/// design B / B2), **and** the full set of self-contained application
/// bundles — every discovered program's signed `AppInfo` + `Run` rxe beside
/// its `Help/` tree (`plans/APPS.md` deliverable 8) — with headroom, while
/// staying trivial against the whole-disk image (only non-zero sectors are
/// planted on the backing file).
pub const SYSTEM_SECTORS: u64 = 65_536;

/// First sector of the encrypted `RustFS` root partition: directly after
/// the `/System` partition.
pub const ROOT_LBA: u64 = SYSTEM_LBA + SYSTEM_SECTORS;

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

/// Author the read-only `/System` partition: format a small `RustFS`
/// volume under the non-secret well-known [`SYSTEM_VOLUME_KEY`], lay the
/// `/System` skeleton at its root (`Drivers` plus `Security`), and
/// plant the design-B signed driver `drivers` into its `Drivers/` store —
/// the layout `tools/mkimage::build_system_partition` writes. The kernel mounts it read-only and autoloads the store **before**
/// unlocking the encrypted root (`plans/PI.md` design B / B2), so the store
/// — not the encrypted root — carries the boot drivers.
///
/// Each driver is `(path_components, bytes)` where `path_components` is the
/// bundle leaf's path **relative to this `/System` volume's root** (the
/// volume's root *is* `/System`, so the `/System/Drivers/` store is at
/// the volume-relative `Drivers/…`, e.g.
/// `&[b"Drivers", b"input", b"virtio_kbd", b"Run"]`).
///
/// `apps` is the application-bundle file set in the same shape — every
/// program's signed `AppInfo` + `Run` at its volume-relative store path
/// (e.g. `&[b"Apps", b"ls.app", b"Run"]`), planted beside the `Help/`
/// trees below so each on-disk bundle is complete and self-contained
/// (`plans/APPS.md` deliverable 8). The caller composes and signs the
/// files; this fixture only plants bytes. Returns the partition's
/// on-disk bytes.
fn build_system_partition(
    drivers: &[(&[&[u8]], &[u8])],
    apps: &[(&[&[u8]], &[u8])],
) -> Result<Vec<u8>, DriverError> {
    let mut fs = RustFs::format(
        MemDisk::new(SYSTEM_SECTORS),
        64,
        &SYSTEM_VOLUME_KEY,
        &mut FixtureEntropy { next: 3 },
    )?;
    let root = fs.root();
    let security = fs.create(root, b"Security", NodeKind::Directory)?;
    fs.create(security, b"Keys", NodeKind::Directory)?;
    fs.create(security, b"Policy", NodeKind::Directory)?;
    fs.create(root, b"Drivers", NodeKind::Directory)?;
    for (components, bytes) in drivers {
        root_image::plant_nested_file(&mut fs, root, components, bytes)?;
    }
    // The system app store's bundle data, exactly as `tools/mkimage` plants
    // it: each command app's internationalised Help/ tree, discovered from
    // the bundle's own on-disk `Help/` source (`rustos_syshelp`) — never a
    // hand-maintained list here — so the session vertical reads the same
    // bytes a real image ships.
    for doc in rustos_syshelp::HELP_FILES {
        let components: [&[u8]; 5] = [
            b"Apps",
            doc.bundle.as_bytes(),
            b"Help",
            doc.locale.as_bytes(),
            doc.file.as_bytes(),
        ];
        root_image::plant_nested_file(&mut fs, root, &components, doc.bytes)?;
    }
    // Each program's signed `AppInfo` + `Run` land beside its `Help/` tree
    // (`Apps/<name>.app/…`, `Services/<name>.app/…`), exactly as
    // `tools/mkimage` plants them, so every on-disk bundle the vertical
    // browses is complete and self-contained.
    for (components, bytes) in apps {
        root_image::plant_nested_file(&mut fs, root, components, bytes)?;
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
/// panicked so the builder holds to in every path it links
/// into.
pub fn build_image() -> Result<Vec<u8>, DriverError> {
    build_image_with_contents(&[], &[], PASSPHRASE)
}

/// Build the whole-disk encrypted-root image with the signed application
/// bundles `apps` planted into the read-only `/System` app/service stores
/// beside their `Help/` trees — each `(path_components, bytes)` a
/// volume-relative bundle file (e.g. `&[b"Apps", b"ls.app", b"AppInfo"]`),
/// composed and signed by the caller (`plans/APPS.md` deliverable 8).
///
/// # Errors
///
/// Propagates any [`DriverError`] from the underlying whole-disk assembly
/// (surfaced rather than panicked).
pub fn build_image_with_apps(apps: &[(&[&[u8]], &[u8])]) -> Result<Vec<u8>, DriverError> {
    build_image_with_contents(&[], apps, PASSPHRASE)
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
/// Propagates any [`DriverError`] from descriptor provisioning, FAT/`RustFS`
/// authoring, or the MBR encode (surfaced rather than panicked).
pub fn build_image_with_passphrase(passphrase: &[u8]) -> Result<Vec<u8>, DriverError> {
    build_image_with_contents(&[], &[], passphrase)
}

/// Build the whole-disk encrypted-root image, additionally planting a set of
/// installed driver bundles into the encrypted root's `/System/Drivers/`
/// store.
///
/// This is [`build_image`] with the discovered driver store populated:
/// each `(path_components, bytes)` is laid into the **read-only `/System`
/// volume**'s `Drivers/` store exactly as a real installation carries it, so
/// the pre-unlock autoload
/// (`rustos_kernel::root_mount::autoload_system_drivers` →
/// `driver_autoload::autoload_from_mounted_root`) discovers, verifies, and
/// spawns the bundle off the `/System` volume *before* the encrypted root is
/// unlocked (design B — the store lives on a volume reachable before the
/// passphrase). [`build_image`] delegates here with no drivers (one authoring path).
///
/// # Errors
///
/// Propagates any [`DriverError`] from descriptor provisioning, FAT/`RustFS`
/// authoring, or the MBR encode (surfaced rather than panicked).
pub fn build_image_with_drivers(drivers: &[(&[&[u8]], &[u8])]) -> Result<Vec<u8>, DriverError> {
    build_image_with_contents(drivers, &[], PASSPHRASE)
}

/// Build the whole-disk encrypted-root image, planting `drivers` and the
/// application-bundle files `apps` into the read-only `/System` stores and
/// encrypting the root under `passphrase`.
///
/// The single authoring path behind [`build_image`],
/// [`build_image_with_drivers`], [`build_image_with_apps`], and
/// [`build_image_with_passphrase`]: they differ only in the planted store
/// contents and the passphrase the root volume key is derived from.
///
/// # Errors
///
/// Propagates any [`DriverError`] from descriptor provisioning, FAT/`RustFS`
/// authoring, or the MBR encode (surfaced rather than panicked).
pub fn build_image_with_contents(
    drivers: &[(&[&[u8]], &[u8])],
    apps: &[(&[&[u8]], &[u8])],
    passphrase: &[u8],
) -> Result<Vec<u8>, DriverError> {
    let (descriptor, key) = provision(passphrase)?;
    let boot = build_boot_partition(&descriptor)?;
    let system = build_system_partition(drivers, apps)?;
    let root = root_image::build_users_root_image_with_key(&key)?;

    let table = mbr::encode(&[
        Partition {
            ty: PartitionType::FatBoot,
            start_lba: BOOT_LBA,
            block_count: FAT_BOOT_SECTORS,
        },
        Partition {
            ty: PartitionType::RustFsSystem,
            start_lba: SYSTEM_LBA,
            block_count: SYSTEM_SECTORS,
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
    let system_at = usize::try_from(SYSTEM_LBA).unwrap_or(0) * SECTOR_BYTES;
    image[system_at..system_at + system.len()].copy_from_slice(&system);
    let root_at = usize::try_from(ROOT_LBA).unwrap_or(0) * SECTOR_BYTES;
    image[root_at..root_at + root.len()].copy_from_slice(&root);
    Ok(image)
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use rustos_partition::{parse_partition_table, PartitionBlock};

    /// The assembled image carries exactly the three design-B partitions,
    /// of the right types, at the documented 1 MiB-aligned offsets.
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

        let system = table
            .first_of_type(PartitionType::RustFsSystem)
            .expect("a read-only /System partition is present");
        assert_eq!(system.start_lba, SYSTEM_LBA);
        assert_eq!(system.block_count, SYSTEM_SECTORS);

        let root = table
            .first_of_type(PartitionType::RustFsRoot)
            .expect("a RustFS root partition is present");
        assert_eq!(root.start_lba, ROOT_LBA);
        assert_eq!(root.block_count, ROOT_SECTORS);
    }

    /// A planted application-bundle file (`Apps/<name>.app/AppInfo` /
    /// `Run`) reads back byte-for-byte beside the bundle's discovered
    /// `Help/` tree, so every on-disk bundle the verticals browse is
    /// complete and self-contained.
    #[test]
    fn planted_app_bundle_files_read_back_beside_the_help_tree() {
        const APPINFO: &[u8] = b"a signed AppInfo manifest's bytes (synthetic)";
        const RUN: &[u8] = b"a Run rxe image's bytes (synthetic)";
        let apps: [(&[&[u8]], &[u8]); 4] = [
            (&[b"Apps", b"ls.app", b"AppInfo"], APPINFO),
            (&[b"Apps", b"ls.app", b"Run"], RUN),
            (&[b"Services", b"login.app", b"AppInfo"], APPINFO),
            (&[b"Services", b"login.app", b"Run"], RUN),
        ];
        let bytes = build_image_with_apps(&apps).expect("the whole-disk image assembles");
        let mut disk = MemDisk { store: bytes };
        let table = parse_partition_table(&mut disk).expect("the MBR parses");
        let system = table
            .first_of_type(PartitionType::RustFsSystem)
            .expect("a /System partition is present");
        let window = PartitionBlock::new(disk, system.start_lba, system.block_count)
            .expect("the /System window is in range");
        let mut sys = RustFs::open_read_only(window, &SYSTEM_VOLUME_KEY)
            .expect("the /System volume mounts read-only under the public key");

        for (components, expected) in &apps {
            let mut node = sys.root();
            for component in *components {
                node = sys.lookup(node, component).expect("store path component");
            }
            let mut buf = [0u8; 64];
            let read = sys.read_at(node, 0, &mut buf).expect("file reads back");
            assert_eq!(&buf[..read], *expected);
        }
        // The command app's bundle also carries its discovered Help/ tree.
        let mut help = sys.root();
        for component in [b"Apps".as_slice(), b"ls.app", b"Help", b"default"] {
            help = sys.lookup(help, component).expect("Help path component");
        }
        sys.lookup(help, b"ls.md")
            .expect("the bundle's default help document is planted beside Run");
    }

    /// The `/System` partition mounts read-only under the non-secret
    /// well-known key and carries the `Drivers` store directory.
    #[test]
    fn the_system_window_mounts_read_only_under_the_public_key() {
        let bytes = build_image().expect("the whole-disk image assembles");
        let mut disk = MemDisk { store: bytes };
        let table = parse_partition_table(&mut disk).expect("the MBR parses");
        let system = table
            .first_of_type(PartitionType::RustFsSystem)
            .expect("a /System partition is present");
        let window = PartitionBlock::new(disk, system.start_lba, system.block_count)
            .expect("the /System window is in range");
        let mut sys = RustFs::open_read_only(window, &SYSTEM_VOLUME_KEY)
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
        use rustos_drv_fs_rustfs::RustFs;

        let bytes = build_image().expect("the whole-disk image assembles");
        let mut disk = MemDisk { store: bytes };
        let table = parse_partition_table(&mut disk).expect("the MBR parses");
        let root = table
            .first_of_type(PartitionType::RustFsRoot)
            .expect("a RustFS root partition is present");

        let (_descriptor, key) = provision(PASSPHRASE).expect("the descriptor provisions");
        let window = PartitionBlock::new(disk, root.start_lba, root.block_count)
            .expect("the root window is in range");
        RustFs::open(window, &key).expect("the root mounts under the descriptor-derived key");
    }
}
