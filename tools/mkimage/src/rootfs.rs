//! `RustFS` root-partition authoring.
//!
//! The root partition is a genuine encrypted `RustFS` volume laid down by
//! the real driver (`rustos-drv-fs-rustfs`) and pre-populated with the
//! authoritative `AGENTS.md` §16 top-level layout: exactly `/System`,
//! `/Users`, `/Apps`, and `/Storage`, plus the fixed `/System` subtree.
//! The user and group databases under `/System/Security`, the first user's
//! home, and the mount policies are the §11 installer's first-boot job —
//! the image ships the skeleton the installer fills in. A **debug** image
//! ([`crate::ImageProfile::Debug`]) additionally seeds a pre-authored
//! `/System/Security/Users` database so the login prompt is usable without
//! running the installer; an installer image ships none.
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

/// Name of the user database file under `/System/Security` (`AGENTS.md`
/// §16.2).
pub const USERS_DB_NAME: &str = "Users";

/// Author the `RustFS` root partition: format `sectors` sectors under
/// `volume_key`, create the §16 directory skeleton, and — when `users_db`
/// is given — write it to `/System/Security/Users`.
///
/// # Errors
///
/// [`MkimageError::RootPartition`] if formatting, any directory creation,
/// or the user-database write fails (including an entropy failure while
/// provisioning the volume's key hierarchy — never a weakly-keyed volume,
/// `AGENTS.md` §5.4).
pub fn build_root_partition(
    sectors: u64,
    volume_key: &VolumeKey,
    entropy: &mut dyn EntropySource,
    users_db: Option<&str>,
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
                    if let Some(text) = users_db {
                        write_users_db(&mut fs, sub_node, text)?;
                    }
                }
            }
        }
    }

    fs.flush().map_err(MkimageError::RootPartition)?;
    Ok(fs.into_block().into_bytes())
}

/// Create `/System/Security/Users` and write `text` into it whole; a short
/// write is a build failure, never a truncated database (`AGENTS.md` §2.9).
fn write_users_db(
    fs: &mut RustFs<MemBlock>,
    security: rustos_abi::driver::filesystem::NodeId,
    text: &str,
) -> Result<(), MkimageError> {
    fs.create(security, USERS_DB_NAME.as_bytes(), NodeKind::RegularFile)
        .map_err(MkimageError::RootPartition)?;
    let written = fs
        .write_at(security, USERS_DB_NAME.as_bytes(), 0, text.as_bytes())
        .map_err(MkimageError::RootPartition)?;
    if written != text.len() {
        return Err(MkimageError::RootPartition(
            rustos_abi::DriverError::DeviceFault,
        ));
    }
    Ok(())
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
        build_root_partition(TEST_SECTORS, &TEST_KEY, &mut TestEntropy(7), None)
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
        assert!(build_root_partition(TEST_SECTORS, &TEST_KEY, &mut NoEntropy, None).is_err());
    }

    #[test]
    fn a_seeded_users_database_is_written_and_reads_back() {
        let text = "rustos-users-v1\n# seeded for the test\n";
        let bytes = build_root_partition(TEST_SECTORS, &TEST_KEY, &mut TestEntropy(7), Some(text))
            .expect("root partition builds");
        let dev = MemBlock::from_bytes(bytes).expect("whole sectors");
        let mut fs = RustFs::open(dev, &TEST_KEY).expect("mounts");
        let root = fs.root();
        let system = fs.lookup(root, b"System").expect("/System exists");
        let security = fs.lookup(system, b"Security").expect("Security exists");
        let users = fs
            .lookup(security, USERS_DB_NAME.as_bytes())
            .expect("Users database exists");
        let mut buf = vec![0u8; text.len() + 16];
        let read = fs
            .read_at(users, 0, &mut buf)
            .expect("Users database reads");
        assert_eq!(&buf[..read], text.as_bytes());
    }

    #[test]
    fn an_unseeded_root_ships_no_users_database() {
        let bytes = build();
        let dev = MemBlock::from_bytes(bytes).expect("whole sectors");
        let mut fs = RustFs::open(dev, &TEST_KEY).expect("mounts");
        let root = fs.root();
        let system = fs.lookup(root, b"System").expect("/System exists");
        let security = fs.lookup(system, b"Security").expect("Security exists");
        assert!(fs.lookup(security, USERS_DB_NAME.as_bytes()).is_err());
    }
}
