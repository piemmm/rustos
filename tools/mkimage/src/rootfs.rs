//! `RustFS` root-partition authoring.
//!
//! The root partition is a genuine encrypted `RustFS` volume laid down by
//! the real driver (`rustos-drv-fs-rustfs`) and pre-populated with the
//! authoritative top-level layout: exactly `/System`, `/Users`, `/Apps`,
//! and `/Storage`. It is the **writable** volume mounted as `/`, so under
//! `/System` it carries **only** the writable-state subtree
//! ([`WRITABLE_SYSTEM_SUBDIRS`]: `Logs`, `Settings`, and `Security`) — the
//! immutable `/System` content (`Kernel`, `Drivers`, `Libraries`, …) lives on
//! the separate read-only `RustFsSystem` volume that is mounted *over* this
//! one at `/System`, so duplicating it here would be dead weight and a second
//! copy that could drift.
//! The user and group databases under `/System/Security`, the first user's
//! home, and the mount policies are the installer's first-boot job —
//! the image ships the skeleton the installer fills in. A **debug** image
//! ([`crate::ImageProfile::Debug`]) additionally seeds a pre-authored
//! `/System/Security/Users` database **and** the matching
//! `/System/Security/Groups` registry so the login prompt is usable and the
//! kernel can build its identity table without running the installer; an
//! installer image ships neither.
//!
//! `RustFS` has no plaintext mode: the volume is provisioned under a
//! caller-supplied volume key, and mounting it requires that key. The
//! image builder draws a fresh random key per image and hands it back to
//! the operator (`crate::build_rpi_image`); it is never stored inside the
//! image.

use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeId, NodeKind};
use rustos_drv_fs_rustfs::{
    plant_nested_file, EntropySource, RustFs, Security, VolumeKey, SYSTEM_VOLUME_KEY,
};

use crate::device::MemBlock;
use crate::MkimageError;

/// The top-level directories. Exactly these four; any
/// other top-level name on a RustOS volume is a defect.
pub const TOP_LEVEL_DIRS: [&str; 4] = ["System", "Users", "Apps", "Storage"];

/// The full `/System` subtree the **read-only** `RustFsSystem` volume ships
/// (`build_system_partition`). `Security` additionally carries its fixed
/// `Keys` and `Policy` subdirectories; the `Users`/`Groups` databases inside
/// it are installer-authored data, not image content.
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

/// The writable-state `/System` subtree the **encrypted root** volume ships
/// (`build_root_partition`). These are the only `/System` paths that resolve
/// to the writable root volume at runtime: `Logs` and `Settings` are the
/// writable exceptions mounted over the read-only `/System`, and `Security`
/// holds the encrypted user/group databases (read by the boot-time
/// `/System/Security` reader off this volume, where the secret belongs — it
/// is never placed on the well-known-keyed `RustFsSystem` volume). The
/// immutable subdirectories in [`SYSTEM_SUBDIRS`] are deliberately absent
/// here; they live only on `RustFsSystem`.
pub const WRITABLE_SYSTEM_SUBDIRS: [&str; 3] = ["Logs", "Settings", "Security"];

/// Number of inodes the root volume is formatted with: ample for the
/// skeleton plus the installer's first-boot output, while trivial against
/// the volume size (`RustFS` allocates inodes from this hint's table).
const ROOT_INODE_HINT: u32 = 4096;

/// Name of the user database file under `/System/Security`.
pub const USERS_DB_NAME: &str = "Users";

/// Name of the group registry file under `/System/Security`.
pub const GROUPS_DB_NAME: &str = "Groups";

/// Name of the per-installation machine-id file under `/System/Security`
/// (`AGENTS.md` §16.2). Its bytes are the raw [`rustos_abi::MACHINE_ID_LEN`]
/// machine-id — non-secret per-installation identity (the RustOS equivalent
/// of `/etc/machine-id`) that the system log binds its stream-genesis to
/// (`plans/SYSLOG.md` §7.1). The journal service reads it at startup.
pub const MACHINE_ID_NAME: &str = "MachineId";

/// Mode for the machine-id file: world-readable, owner-writable (`0o644`).
/// The machine-id is **not** a secret — unlike the log-attestation key it is
/// public per-installation identity, so any principal may read it while only
/// the system user (uid/gid 0) may rewrite it.
const MACHINE_ID_MODE: u32 = 0o644;

/// Name of the per-installation log-attestation key file under
/// `/System/Security/Keys` (`PREREQUISITES.md` P-E). Its bytes are the
/// [`rustos_log::LogAttestationKey`] on-disk image.
pub const LOG_ATTESTATION_KEY_NAME: &str = "LogAttestation";

/// Restrictive mode for the log-attestation key file: owner read/write only
/// (`0o600`). The key is a secret; together with the system-user ownership
/// (uid/gid 0) and the read-only-until-a-holder-exists policy, this keeps it
/// unreadable by any ordinary principal until the journal/attestation
/// principal exists.
const LOG_ATTESTATION_KEY_MODE: u32 = 0o600;

/// Author the `RustFS` root partition: format `sectors` sectors under
/// `volume_key`, create the directory skeleton, and — when `users_db` /
/// `groups_db` are given — write them to `/System/Security/Users` and
/// `/System/Security/Groups`.
///
/// # Errors
///
/// [`MkimageError::RootPartition`] if formatting, any directory creation,
/// or a database write fails (including an entropy failure while
/// provisioning the volume's key hierarchy — never a weakly-keyed volume).
pub fn build_root_partition(
    sectors: u64,
    volume_key: &VolumeKey,
    entropy: &mut dyn EntropySource,
    users_db: Option<&str>,
    groups_db: Option<&str>,
    log_attestation_key: Option<&[u8]>,
    machine_id: Option<&[u8]>,
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
            populate_system_subtree(
                &mut fs,
                node,
                users_db,
                groups_db,
                log_attestation_key,
                machine_id,
            )?;
        }
    }

    fs.flush().map_err(MkimageError::RootPartition)?;
    Ok(fs.into_block().into_bytes())
}

/// Author the read-only, signed-bundle `/System` partition: format
/// `sectors` sectors under the non-secret well-known
/// [`SYSTEM_VOLUME_KEY`] and lay the `/System` subtree
/// **at the volume root** (the volume *is* `/System` once mounted, so its
/// root carries `Kernel`, `Drivers`, … directly).
///
/// This is the design-B pre-unlock store (`plans/PI.md`): it carries no
/// secrets, so it is keyed by the public [`SYSTEM_VOLUME_KEY`] and the
/// kernel mounts it read-only (`RustFs::open_read_only`) *before* the
/// encrypted data root is unlocked. The signed driver bundles land here in
/// the later design-B increments; B1 establishes the volume and its
/// skeleton. The subdirectories are laid down through the one shared
/// `create_system_subdirs` helper used by the encrypted root too. No users database is written here — that secret
/// stays on the encrypted root.
///
/// # Errors
///
/// [`MkimageError::SystemPartition`] if formatting or any directory
/// creation fails (including an entropy failure provisioning the volume's
/// key hierarchy).
pub fn build_system_partition(
    sectors: u64,
    entropy: &mut dyn EntropySource,
    drivers: &[(&[&[u8]], &[u8])],
) -> Result<Vec<u8>, MkimageError> {
    let dev = MemBlock::new(sectors).map_err(MkimageError::SystemPartition)?;
    let mut fs = RustFs::format(dev, ROOT_INODE_HINT, &SYSTEM_VOLUME_KEY, entropy)
        .map_err(MkimageError::SystemPartition)?;
    let root = fs.root();
    create_system_subdirs(&mut fs, root, MkimageError::SystemPartition)?;
    // Lay each signed driver bundle into the read-only `/System` store at its
    // volume-relative path (`Drivers/<class>/<leaf>/Run`), creating any
    // intermediate directory the skeleton did not (the `Drivers`
    // directory already exists, so the shared planter reuses it). This is the
    // on-disk shape the autoload scan reads back; the bundle is
    // already Ed25519-signed against the kernel's trust anchor, so a tampered
    // read-only store fails the load gate closed.
    for (components, bytes) in drivers {
        plant_nested_file(&mut fs, root, components, bytes)
            .map_err(MkimageError::SystemPartition)?;
    }
    fs.flush().map_err(MkimageError::SystemPartition)?;
    Ok(fs.into_block().into_bytes())
}

/// Lay the **writable-state** `/System` subtree under `system` on the
/// encrypted data root: the [`WRITABLE_SYSTEM_SUBDIRS`] (`Logs`, `Settings`,
/// and `Security` with its `Keys`/`Policy`), and — for a debug image — the
/// seeded users database and matching group registry under `Security`. The
/// immutable `/System` content is **not** authored here (it lives on the
/// read-only `RustFsSystem` volume); only what the writable root volume
/// actually backs at runtime is laid down.
fn populate_system_subtree(
    fs: &mut RustFs<MemBlock>,
    system: NodeId,
    users_db: Option<&str>,
    groups_db: Option<&str>,
    log_attestation_key: Option<&[u8]>,
    machine_id: Option<&[u8]>,
) -> Result<(), MkimageError> {
    for sub in WRITABLE_SYSTEM_SUBDIRS {
        let node = fs
            .create(system, sub.as_bytes(), NodeKind::Directory)
            .map_err(MkimageError::RootPartition)?;
        if sub == "Security" {
            create_security_subdirs(fs, node, MkimageError::RootPartition)?;
        }
    }
    if users_db.is_some() || groups_db.is_some() {
        let security = fs
            .lookup(system, b"Security")
            .map_err(MkimageError::RootPartition)?;
        if let Some(text) = users_db {
            write_security_file(fs, security, USERS_DB_NAME, text)?;
        }
        if let Some(text) = groups_db {
            write_security_file(fs, security, GROUPS_DB_NAME, text)?;
        }
    }
    if let Some(key_bytes) = log_attestation_key {
        let security = fs
            .lookup(system, b"Security")
            .map_err(MkimageError::RootPartition)?;
        let keys = fs
            .lookup(security, b"Keys")
            .map_err(MkimageError::RootPartition)?;
        write_key_file(fs, keys, LOG_ATTESTATION_KEY_NAME, key_bytes)?;
    }
    if let Some(id_bytes) = machine_id {
        let security = fs
            .lookup(system, b"Security")
            .map_err(MkimageError::RootPartition)?;
        write_machine_id_file(fs, security, MACHINE_ID_NAME, id_bytes)?;
    }
    Ok(())
}

/// Create `/System/Security/<name>`, write the non-secret machine-id `bytes`
/// into it whole, and set it world-readable, system-user-owned
/// ([`MACHINE_ID_MODE`]). A short write is a build failure, never a truncated
/// id. Unlike the log-attestation key the machine-id is public identity, so it
/// is readable by any principal (only the system user may rewrite it).
fn write_machine_id_file(
    fs: &mut RustFs<MemBlock>,
    security: NodeId,
    name: &str,
    bytes: &[u8],
) -> Result<(), MkimageError> {
    let file = fs
        .create(security, name.as_bytes(), NodeKind::RegularFile)
        .map_err(MkimageError::RootPartition)?;
    let written = fs
        .write_at(security, name.as_bytes(), 0, bytes)
        .map_err(MkimageError::RootPartition)?;
    if written != bytes.len() {
        return Err(MkimageError::RootPartition(
            rustos_abi::DriverError::DeviceFault,
        ));
    }
    fs.set_security(file, Security::new(MACHINE_ID_MODE, 0, 0))
        .map_err(MkimageError::RootPartition)?;
    Ok(())
}

/// Create the [`SYSTEM_SUBDIRS`] under `system`, with `Keys` and `Policy`
/// inside `Security`. The one definition both the encrypted-root and the
/// `/System`-volume authoring paths reuse; `wrap` tags
/// the failure with the partition the caller is authoring.
fn create_system_subdirs(
    fs: &mut RustFs<MemBlock>,
    system: NodeId,
    wrap: fn(rustos_abi::DriverError) -> MkimageError,
) -> Result<(), MkimageError> {
    for sub in SYSTEM_SUBDIRS {
        let sub_node = fs
            .create(system, sub.as_bytes(), NodeKind::Directory)
            .map_err(wrap)?;
        if sub == "Security" {
            create_security_subdirs(fs, sub_node, wrap)?;
        }
    }
    Ok(())
}

/// Create the fixed `Keys` and `Policy` subdirectories under a `Security`
/// node. The one definition both the read-only `/System` volume and the
/// encrypted root's writable-state subtree author their `Security/{Keys,
/// Policy}` through, so the two cannot drift; `wrap` tags the failure with
/// the partition the caller is authoring.
fn create_security_subdirs(
    fs: &mut RustFs<MemBlock>,
    security: NodeId,
    wrap: fn(rustos_abi::DriverError) -> MkimageError,
) -> Result<(), MkimageError> {
    for sec in ["Keys", "Policy"] {
        fs.create(security, sec.as_bytes(), NodeKind::Directory)
            .map_err(wrap)?;
    }
    Ok(())
}

/// Create `/System/Security/<name>` and write `text` into it whole; a short
/// write is a build failure, never a truncated database. The one definition
/// the users-database and group-registry authoring both go through, so the
/// two cannot drift in how a security file is laid down.
fn write_security_file(
    fs: &mut RustFs<MemBlock>,
    security: rustos_abi::driver::filesystem::NodeId,
    name: &str,
    text: &str,
) -> Result<(), MkimageError> {
    fs.create(security, name.as_bytes(), NodeKind::RegularFile)
        .map_err(MkimageError::RootPartition)?;
    let written = fs
        .write_at(security, name.as_bytes(), 0, text.as_bytes())
        .map_err(MkimageError::RootPartition)?;
    if written != text.len() {
        return Err(MkimageError::RootPartition(
            rustos_abi::DriverError::DeviceFault,
        ));
    }
    Ok(())
}

/// Create `/System/Security/Keys/<name>`, write the secret `bytes` into it
/// whole, and lock it down to system-user-owned, owner-read/write-only
/// ([`LOG_ATTESTATION_KEY_MODE`]). A short write is a build failure, never a
/// truncated key. The restrictive security record is the only thing gating
/// the secret until the journal/attestation principal exists (no new
/// capability is minted ahead of that holder).
fn write_key_file(
    fs: &mut RustFs<MemBlock>,
    keys: NodeId,
    name: &str,
    bytes: &[u8],
) -> Result<(), MkimageError> {
    let file = fs
        .create(keys, name.as_bytes(), NodeKind::RegularFile)
        .map_err(MkimageError::RootPartition)?;
    let written = fs
        .write_at(keys, name.as_bytes(), 0, bytes)
        .map_err(MkimageError::RootPartition)?;
    if written != bytes.len() {
        return Err(MkimageError::RootPartition(
            rustos_abi::DriverError::DeviceFault,
        ));
    }
    // System-user-owned, owner-read/write-only: an ordinary principal cannot
    // read the key, and the read-only-`/System` policy plus this mode gate the
    // secret until the journal/attestation principal exists. The kernel refuses
    // the record otherwise (fail closed).
    fs.set_security(file, Security::new(LOG_ATTESTATION_KEY_MODE, 0, 0))
        .map_err(MkimageError::RootPartition)?;
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
        build_root_partition(
            TEST_SECTORS,
            &TEST_KEY,
            &mut TestEntropy(7),
            None,
            None,
            None,
            None,
        )
        .expect("root partition builds")
    }

    #[test]
    fn lays_out_the_writable_state_skeleton_only() {
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
        // The encrypted root carries ONLY the writable-state /System subtree;
        // the immutable content lives on the read-only RustFsSystem volume.
        for sub in WRITABLE_SYSTEM_SUBDIRS {
            fs.lookup(system, sub.as_bytes())
                .unwrap_or_else(|_| panic!("/System/{sub} exists"));
        }
        let security = fs.lookup(system, b"Security").expect("Security exists");
        fs.lookup(security, b"Keys").expect("Security/Keys exists");
        fs.lookup(security, b"Policy")
            .expect("Security/Policy exists");

        // The immutable subdirectories are deliberately absent on the
        // writable root — duplicating them here is the "two /System folders"
        // defect this layering removes. They are reached at `/System` through
        // the read-only RustFsSystem volume mounted over `/`.
        for immutable in ["Kernel", "Drivers", "Libraries", "Fonts", "Services"] {
            assert!(
                fs.lookup(system, immutable.as_bytes()).is_err(),
                "/System/{immutable} must NOT exist on the writable root volume"
            );
        }
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
        assert!(build_root_partition(
            TEST_SECTORS,
            &TEST_KEY,
            &mut NoEntropy,
            None,
            None,
            None,
            None
        )
        .is_err());
    }

    #[test]
    fn a_seeded_users_database_is_written_and_reads_back() {
        let text = "rustos-users-v1\n# seeded for the test\n";
        let bytes = build_root_partition(
            TEST_SECTORS,
            &TEST_KEY,
            &mut TestEntropy(7),
            Some(text),
            None,
            None,
            None,
        )
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
    fn a_seeded_group_registry_is_written_and_reads_back() {
        let text = "rustos-groups-v1\nwheel:0\n";
        let bytes = build_root_partition(
            TEST_SECTORS,
            &TEST_KEY,
            &mut TestEntropy(7),
            None,
            Some(text),
            None,
            None,
        )
        .expect("root partition builds");
        let dev = MemBlock::from_bytes(bytes).expect("whole sectors");
        let mut fs = RustFs::open(dev, &TEST_KEY).expect("mounts");
        let root = fs.root();
        let system = fs.lookup(root, b"System").expect("/System exists");
        let security = fs.lookup(system, b"Security").expect("Security exists");
        let groups = fs
            .lookup(security, GROUPS_DB_NAME.as_bytes())
            .expect("Groups registry exists");
        let mut buf = vec![0u8; text.len() + 16];
        let read = fs
            .read_at(groups, 0, &mut buf)
            .expect("Groups registry reads");
        assert_eq!(&buf[..read], text.as_bytes());
    }

    #[test]
    fn an_unseeded_root_ships_no_users_or_groups_database() {
        let bytes = build();
        let dev = MemBlock::from_bytes(bytes).expect("whole sectors");
        let mut fs = RustFs::open(dev, &TEST_KEY).expect("mounts");
        let root = fs.root();
        let system = fs.lookup(root, b"System").expect("/System exists");
        let security = fs.lookup(system, b"Security").expect("Security exists");
        assert!(fs.lookup(security, USERS_DB_NAME.as_bytes()).is_err());
        assert!(fs.lookup(security, GROUPS_DB_NAME.as_bytes()).is_err());
        // An unseeded (installer-shaped) root also ships no log-attestation
        // key: the first-boot installer generates the per-installation key,
        // never the image.
        let keys = fs.lookup(security, b"Keys").expect("Keys exists");
        assert!(fs
            .lookup(keys, LOG_ATTESTATION_KEY_NAME.as_bytes())
            .is_err());
    }

    #[test]
    fn a_seeded_log_attestation_key_is_written_locked_down_and_parses() {
        use rustos_abi::driver::filesystem::FilesystemSecurity;
        use rustos_log::{LogAttestationKey, LOG_ATTESTATION_KEY_FILE_LEN};

        // A debug-shaped key image: a real `LogAttestationKey` on-disk blob.
        let key_file = LogAttestationKey::from_key([0x5A; rustos_log::LOG_ATTESTATION_KEY_LEN])
            .to_file_bytes()
            .to_vec();
        let bytes = build_root_partition(
            TEST_SECTORS,
            &TEST_KEY,
            &mut TestEntropy(7),
            None,
            None,
            Some(&key_file),
            None,
        )
        .expect("root partition builds");
        let dev = MemBlock::from_bytes(bytes).expect("whole sectors");
        let mut fs = RustFs::open(dev, &TEST_KEY).expect("mounts");
        let root = fs.root();
        let system = fs.lookup(root, b"System").expect("/System exists");
        let security = fs.lookup(system, b"Security").expect("Security exists");
        let keys = fs.lookup(security, b"Keys").expect("Keys exists");
        let key_node = fs
            .lookup(keys, LOG_ATTESTATION_KEY_NAME.as_bytes())
            .expect("log-attestation key exists");
        // The bytes read back are the exact on-disk key image and parse.
        let mut buf = [0u8; LOG_ATTESTATION_KEY_FILE_LEN];
        let read = fs.read_at(key_node, 0, &mut buf).expect("key reads");
        assert_eq!(read, LOG_ATTESTATION_KEY_FILE_LEN);
        assert_eq!(&buf[..], &key_file[..]);
        assert!(LogAttestationKey::from_file_bytes(&buf).is_ok());
        // Locked down: system-user-owned, owner-read/write-only.
        let sec = fs.security(key_node).expect("security present");
        assert_eq!(sec.mode, LOG_ATTESTATION_KEY_MODE);
        assert_eq!(sec.uid, 0);
        assert_eq!(sec.gid, 0);
    }

    #[test]
    fn a_seeded_machine_id_is_written_world_readable_and_reads_back() {
        use rustos_abi::driver::filesystem::FilesystemSecurity;
        use rustos_abi::MACHINE_ID_LEN;

        let id = [0xA7u8; MACHINE_ID_LEN];
        let bytes = build_root_partition(
            TEST_SECTORS,
            &TEST_KEY,
            &mut TestEntropy(7),
            None,
            None,
            None,
            Some(&id),
        )
        .expect("root partition builds");
        let dev = MemBlock::from_bytes(bytes).expect("whole sectors");
        let mut fs = RustFs::open(dev, &TEST_KEY).expect("mounts");
        let root = fs.root();
        let system = fs.lookup(root, b"System").expect("/System exists");
        let security = fs.lookup(system, b"Security").expect("Security exists");
        let node = fs
            .lookup(security, MACHINE_ID_NAME.as_bytes())
            .expect("machine-id exists");
        // The bytes read back are exactly the provisioned machine-id.
        let mut buf = [0u8; MACHINE_ID_LEN];
        let read = fs.read_at(node, 0, &mut buf).expect("machine-id reads");
        assert_eq!(read, MACHINE_ID_LEN);
        assert_eq!(&buf[..], &id[..]);
        // Non-secret public identity: world-readable, system-user-owned.
        let sec = fs.security(node).expect("security present");
        assert_eq!(sec.mode, MACHINE_ID_MODE);
        assert_eq!(sec.uid, 0);
        assert_eq!(sec.gid, 0);
    }

    #[test]
    fn an_unseeded_root_ships_no_machine_id() {
        let bytes = build();
        let dev = MemBlock::from_bytes(bytes).expect("whole sectors");
        let mut fs = RustFs::open(dev, &TEST_KEY).expect("mounts");
        let root = fs.root();
        let system = fs.lookup(root, b"System").expect("/System exists");
        let security = fs.lookup(system, b"Security").expect("Security exists");
        // An installer-shaped root mints its machine-id at first boot, never
        // in the image.
        assert!(fs.lookup(security, MACHINE_ID_NAME.as_bytes()).is_err());
    }
}
