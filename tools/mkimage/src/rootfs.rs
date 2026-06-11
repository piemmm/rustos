//! `RustFS` root-partition authoring.
//!
//! The root partition is a genuine encrypted `RustFS` volume laid down by
//! the real driver (`rustos-drv-fs-rustfs`) and pre-populated with the
//! authoritative `AGENTS.md` §16 top-level layout: exactly `/System`,
//! `/Users`, `/Apps`, and `/Storage`, plus the fixed `/System` subtree.
//! The user and group databases under `/System/Security`, the first user's
//! home, and the mount policies are the §11 installer's first-boot job —
//! the image ships the skeleton the installer fills in.
//!
//! `RustFS` has no plaintext mode: the volume is provisioned under a
//! caller-supplied volume key, and mounting it requires that key. The
//! image builder draws a fresh random key per image and hands it back to
//! the operator (`crate::build_rpi_image`); it is never stored inside the
//! image.

use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeKind};
use rustos_drv_fs_rustfs::{EntropySource, RustFs, VolumeKey};

use crate::device::MemBlock;
use crate::MkimageError;

/// The `AGENTS.md` §16.1 top-level directories. Exactly these four; any
/// other top-level name on a RustOS volume is a defect.
pub const TOP_LEVEL_DIRS: [&str; 4] = ["System", "Users", "Apps", "Storage"];

/// The `AGENTS.md` §16.2 `/System` subtree the image ships. `Security`
/// additionally carries its fixed `Keys` and `Policy` subdirectories; the
/// `Users`/`Groups` databases inside it are installer-authored data, not
/// image content.
pub const SYSTEM_SUBDIRS: [&str; 12] = [
    "Kernel",
    "Drivers",
    "Libraries",
    "Fonts",
    "Graphics",
    "Audio",
    "Network",
    "Security",
    "Printing",
    "Logs",
    "Settings",
    "Services",
];

/// Number of inodes the root volume is formatted with: ample for the
/// skeleton plus the installer's first-boot output, while trivial against
/// the volume size (`RustFS` allocates inodes from this hint's table).
const ROOT_INODE_HINT: u32 = 4096;

/// Author the `RustFS` root partition: format `sectors` sectors under
/// `volume_key` and create the §16 directory skeleton.
///
/// # Errors
///
/// [`MkimageError::RootPartition`] if formatting or any directory
/// creation fails (including an entropy failure while provisioning the
/// volume's key hierarchy — never a weakly-keyed volume, `AGENTS.md` §5.4).
pub fn build_root_partition(
    sectors: u64,
    volume_key: &VolumeKey,
    entropy: &mut dyn EntropySource,
) -> Result<Vec<u8>, MkimageError> {
    let dev = MemBlock::new(sectors).map_err(MkimageError::RootPartition)?;
    let mut fs = RustFs::format(dev, ROOT_INODE_HINT, volume_key, entropy)
        .map_err(MkimageError::RootPartition)?;
    let root = fs.root();

    for name in TOP_LEVEL_DIRS {
        let node = fs
            .create(root, name.as_bytes(), NodeKind::Directory)
            .map_err(MkimageError::RootPartition)?;
        if name == "System" {
            for sub in SYSTEM_SUBDIRS {
                let sub_node = fs
                    .create(node, sub.as_bytes(), NodeKind::Directory)
                    .map_err(MkimageError::RootPartition)?;
                if sub == "Security" {
                    for sec in ["Keys", "Policy"] {
                        fs.create(sub_node, sec.as_bytes(), NodeKind::Directory)
                            .map_err(MkimageError::RootPartition)?;
                    }
                }
            }
        }
    }

    fs.flush().map_err(MkimageError::RootPartition)?;
    Ok(fs.into_block().into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::device::SECTOR_BYTES;
    use rustos_abi::DriverError;

    const TEST_SECTORS: u64 = 131_072; // 64 MiB, the production root size.
    const TEST_KEY: VolumeKey = [0x42; rustos_drv_fs_rustfs::VOLUME_KEY_LEN];

    /// Deterministic test entropy; production uses the host RNG.
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

    fn build() -> Vec<u8> {
        build_root_partition(TEST_SECTORS, &TEST_KEY, &mut TestEntropy(7))
            .expect("root partition builds")
    }

    #[test]
    fn lays_out_the_section_16_skeleton() {
        let bytes = build();
        assert_eq!(
            bytes.len(),
            usize::try_from(TEST_SECTORS).expect("fits") * SECTOR_BYTES
        );

        let dev = MemBlock::from_bytes(bytes).expect("whole sectors");
        let mut fs = RustFs::open(dev, &TEST_KEY).expect("the volume mounts under its key");
        let root = fs.root();
        for name in TOP_LEVEL_DIRS {
            fs.lookup(root, name.as_bytes())
                .unwrap_or_else(|_| panic!("/{name} exists"));
        }
        let system = fs.lookup(root, b"System").expect("/System exists");
        for sub in SYSTEM_SUBDIRS {
            fs.lookup(system, sub.as_bytes())
                .unwrap_or_else(|_| panic!("/System/{sub} exists"));
        }
        let security = fs.lookup(system, b"Security").expect("Security exists");
        fs.lookup(security, b"Keys").expect("Security/Keys exists");
        fs.lookup(security, b"Policy")
            .expect("Security/Policy exists");
    }

    #[test]
    fn the_wrong_key_is_refused() {
        let bytes = build();
        let dev = MemBlock::from_bytes(bytes).expect("whole sectors");
        let wrong: VolumeKey = [0x43; rustos_drv_fs_rustfs::VOLUME_KEY_LEN];
        assert_eq!(
            RustFs::open(dev, &wrong).err(),
            Some(DriverError::PermissionDenied)
        );
    }

    #[test]
    fn a_failed_entropy_draw_fails_the_build_closed() {
        struct NoEntropy;
        impl EntropySource for NoEntropy {
            fn fill(&mut self, _out: &mut [u8]) -> Result<(), DriverError> {
                Err(DriverError::DeviceFault)
            }
        }
        assert!(build_root_partition(TEST_SECTORS, &TEST_KEY, &mut NoEntropy).is_err());
    }
}
