//! `ARXFS` root-partition authoring.
//!
//! The root partition is a genuine encrypted `ARXFS` volume laid down by
//! the real driver (`tairix-drv-fs-arxfs`) and pre-populated with the
//! authoritative top-level layout: exactly `/System`, `/Users`, `/Apps`,
//! and `/Storage`. It is the **writable** volume mounted as `/`, so under
//! `/System` it carries **only** the writable-state subtree
//! ([`WRITABLE_SYSTEM_SUBDIRS`]: `Logs`, `Settings`, and `Security`) — the
//! immutable `/System` content (`Kernel`, `Drivers`, `Libraries`, …) lives on
//! the separate read-only `ARXFSSystem` volume that is mounted *over* this
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
//! `ARXFS` has no plaintext mode: the volume is provisioned under a
//! caller-supplied volume key, and mounting it requires that key. The
//! image builder draws a fresh random key per image and hands it back to
//! the operator (`crate::build_rpi_image`); it is never stored inside the
//! image.

use tairix_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeId, NodeKind};
use tairix_drv_fs_arxfs::{
    plant_nested_file, EntropySource, Security, VolumeKey, ARXFS, SYSTEM_VOLUME_KEY,
};
use tairix_users::{
    appdata_root_security, appdata_transit_security, APPDATA_ROOT, APPDATA_ROOT_PARENTS, HOME_MODE,
    HOME_SUBDIRS,
};

use crate::device::MemBlock;
use crate::MkimageError;

/// The top-level directories. Exactly these four; any
/// other top-level name on a TAIRiX volume is a defect.
pub const TOP_LEVEL_DIRS: [&str; 4] = ["System", "Users", "Apps", "Storage"];

/// The full `/System` subtree the **read-only** `ARXFSSystem` volume ships
/// (`build_system_partition`). `Security` additionally carries its fixed
/// `Keys` and `Policy` subdirectories; the `Users`/`Groups` databases inside
/// it are installer-authored data, not image content. `Apps` is the system
/// app store and `Services` the service store: the OS-provided programs'
/// self-contained bundles — each bundle's signed `AppInfo` + `Run` (composed
/// by the image pipeline's caller) planted beside its internationalised
/// `Help/` tree, discovered from the bundle's own on-disk source and planted
/// from `tairix_syshelp::HELP_FILES`.
pub const SYSTEM_SUBDIRS: [&str; 13] = [
    "Kernel",
    "Apps",
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
/// is never placed on the well-known-keyed `ARXFSSystem` volume). The
/// immutable subdirectories in [`SYSTEM_SUBDIRS`] are deliberately absent
/// here; they live only on `ARXFSSystem`.
pub const WRITABLE_SYSTEM_SUBDIRS: [&str; 3] = ["Logs", "Settings", "Security"];

/// Number of inodes the root volume is formatted with: ample for the
/// skeleton plus the installer's first-boot output, while trivial against
/// the volume size (`ARXFS` allocates inodes from this hint's table).
const ROOT_INODE_HINT: u32 = 4096;

/// Name of the user database file under `/System/Security`.
pub const USERS_DB_NAME: &str = "Users";

/// Name of the group registry file under `/System/Security`.
pub const GROUPS_DB_NAME: &str = "Groups";

/// Name of the writable-Settings subdirectory that holds the network
/// configuration store (`/System/Settings/Network`, `plans/NETWORK.md` §6.1).
pub const NETWORK_SETTINGS_DIR: &str = "Network";

/// Name of the per-interface network-configuration document under
/// `/System/Settings/Network` (`network.conf`). Its content is the image
/// builder's ([`RootSeed::network_conf`]); the first-boot installer, or
/// `configure`, later rewrites it through the one `tairix_netconfig` engine.
pub const NETWORK_CONF_NAME: &str = "network.conf";

/// Name of the per-installation machine-id file under `/System/Security`. Its
/// bytes are the raw [`tairix_abi::MACHINE_ID_LEN`] machine-id — non-secret
/// per-installation identity (the TAIRiX equivalent of `/etc/machine-id`) that
/// the system log binds its stream-genesis to (`plans/SYSLOG.md` §7.1). The
/// journal service reads it at startup.
pub const MACHINE_ID_NAME: &str = "MachineId";

/// Mode for the machine-id file: world-readable, owner-writable (`0o644`).
/// The machine-id is **not** a secret — unlike the log-attestation key it is
/// public per-installation identity, so any principal may read it while only
/// the system user (uid/gid 0) may rewrite it.
const MACHINE_ID_MODE: u32 = 0o644;

/// Name of the per-installation log-attestation key file under
/// `/System/Security/Keys` (`PREREQUISITES.md` P-E). Its bytes are the
/// [`tairix_log::LogAttestationKey`] on-disk image.
pub const LOG_ATTESTATION_KEY_NAME: &str = "LogAttestation";

/// Restrictive mode for the log-attestation key file: owner read/write only
/// (`0o600`). The key is a secret; together with the system-user ownership
/// (uid/gid 0) and the read-only-until-a-holder-exists policy, this keeps it
/// unreadable by any ordinary principal until the journal/attestation
/// principal exists.
const LOG_ATTESTATION_KEY_MODE: u32 = 0o600;

/// Everything seeded onto the encrypted root beyond the directory
/// skeleton. Every image profile seeds both account databases (the
/// canonical default system/service set at minimum, `plans/USERS.md`), so
/// they are not optional; the home directories follow the seeded
/// interactive accounts, and only the debug-only log-attestation key and
/// machine-id are optional.
pub struct RootSeed<'a> {
    /// The `/System/Security/Users` `users-v1` text.
    pub users_db: &'a str,
    /// The `/System/Security/Groups` `groups-v1` text.
    pub groups_db: &'a str,
    /// One `(username, uid, gid)` per seeded interactive account: its
    /// `/Users/<username>` home, provisioned account-owned and owner-only
    /// — a recorded home is a real inode, never a dangling path.
    pub home_dirs: &'a [(&'a str, u32, u32)],
    /// The debug image's baked log-attestation key file bytes, if any.
    pub log_attestation_key: Option<&'a [u8]>,
    /// The debug image's baked non-secret machine-id bytes, if any.
    pub machine_id: Option<&'a [u8]>,
    /// The machine-wide program-library catalog document
    /// (`/System/Settings/ProgramLibrary/library.conf`), derived from the
    /// planted bundles' own manifests ([`crate::library::library_catalog`])
    /// — the canonical empty document when no planted application lists
    /// itself.
    pub library_conf: &'a str,
    /// The per-interface network-configuration document
    /// (`/System/Settings/Network/network.conf`).
    ///
    /// The image builder supplies it, so *which* interfaces an image manages
    /// stays a property of the image being built rather than of this
    /// board-neutral writer: a platform image whose NIC sits at a known bus
    /// location ships an addressing default keyed to it, while an image with
    /// no such NIC ships the canonical empty document ("no managed
    /// interfaces beyond loopback"). Either way it is authored and validated
    /// through the one `tairix_netconfig` engine `netstack` reads it with, so
    /// a shipped default can never fail the parser.
    pub network_conf: &'a str,
}

/// Author the `ARXFS` root partition: format `sectors` sectors under
/// `volume_key`, create the directory skeleton, and lay down everything
/// `seed` describes ([`RootSeed`]).
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
    seed: &RootSeed<'_>,
) -> Result<Vec<u8>, MkimageError> {
    let dev = MemBlock::new(sectors).map_err(MkimageError::RootPartition)?;
    let mut fs = ARXFS::format(dev, ROOT_INODE_HINT, volume_key, entropy)
        .map_err(MkimageError::RootPartition)?;
    let root = fs.root();

    for name in TOP_LEVEL_DIRS {
        let node = fs
            .create(root, name.as_bytes(), NodeKind::Directory)
            .map_err(MkimageError::RootPartition)?;
        if name == "System" {
            populate_system_subtree(&mut fs, node, seed)?;
        }
        if name == "Users" {
            for (username, uid, gid) in seed.home_dirs {
                create_home_dir(&mut fs, node, username, *uid, *gid)?;
            }
        }
    }

    fs.flush().map_err(MkimageError::RootPartition)?;
    Ok(fs.into_block().into_bytes())
}

/// Create `/Users/<username>` owned by `(uid, gid)`, owner-only
/// ([`HOME_MODE`]) and carrying the fixed home shape ([`HOME_SUBDIRS`]) —
/// the very layout `users_admin` provisions a new account's home with, read
/// from the one shared definition so a seeded account and a created one can
/// never get different homes.
///
/// The two [`APPDATA_ROOT_PARENTS`] additionally carry the gated per-app data
/// root, created here rather than on first use: it is owned by the app-data
/// service, so nothing that runs as the user could ever create it, and an
/// account whose home lacked it would have no store at all.
fn create_home_dir(
    fs: &mut ARXFS<MemBlock>,
    users: NodeId,
    username: &str,
    uid: u32,
    gid: u32,
) -> Result<(), MkimageError> {
    let transit = appdata_transit_security(uid, gid).map_err(MkimageError::RootPartition)?;
    let home = fs
        .create(users, username.as_bytes(), NodeKind::Directory)
        .map_err(MkimageError::RootPartition)?;
    fs.set_security(home, transit)
        .map_err(MkimageError::RootPartition)?;
    for name in HOME_SUBDIRS {
        let node = fs
            .create(home, name.as_bytes(), NodeKind::Directory)
            .map_err(MkimageError::RootPartition)?;
        if APPDATA_ROOT_PARENTS.contains(&name) {
            fs.set_security(node, transit)
                .map_err(MkimageError::RootPartition)?;
            let root = fs
                .create(node, APPDATA_ROOT.as_bytes(), NodeKind::Directory)
                .map_err(MkimageError::RootPartition)?;
            fs.set_security(root, appdata_root_security())
                .map_err(MkimageError::RootPartition)?;
        } else {
            fs.set_security(node, Security::new(HOME_MODE, uid, gid))
                .map_err(MkimageError::RootPartition)?;
        }
    }
    Ok(())
}

/// Author the read-only, signed-bundle `/System` partition: format
/// `sectors` sectors under the non-secret well-known
/// [`SYSTEM_VOLUME_KEY`] and lay the `/System` subtree
/// **at the volume root** (the volume *is* `/System` once mounted, so its
/// root carries `Kernel`, `Drivers`, … directly).
///
/// This is the design-B pre-unlock store (`plans/PI.md`): it carries no
/// secrets, so it is keyed by the public [`SYSTEM_VOLUME_KEY`] and the
/// kernel mounts it read-only (`ARXFS::open_read_only`) *before* the
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
    apps: &[(&[&[u8]], &[u8])],
) -> Result<Vec<u8>, MkimageError> {
    let dev = MemBlock::new(sectors).map_err(MkimageError::SystemPartition)?;
    let mut fs = ARXFS::format(dev, ROOT_INODE_HINT, &SYSTEM_VOLUME_KEY, entropy)
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
    // The system payload ships on every image through the one shared walk
    // (`tairix_syshelp::plant_system_payload`): each command app's
    // internationalised `Help/` tree and its `Resources/` files, discovered
    // from the bundle's own on-disk sources, plus the desktop's graphics
    // assets (the icon masters, and the wallpaper masters under their own
    // category directories) planted under `Graphics/`. Driving the
    // walk here — and, identically, in the QEMU image fixture — from one
    // definition means the two planters can never lay down a different set of
    // files, and a new help document, resource, or icon ships without editing
    // this file (never a hand-maintained per-bundle list). The signed
    // `AppInfo` content hash covers a bundle's help and resources, so a
    // tampered one fails the load gate closed.
    tairix_syshelp::plant_system_payload(|components, bytes| {
        plant_nested_file(&mut fs, root, components, bytes).map_err(MkimageError::SystemPartition)
    })?;
    // Each program's signed `AppInfo` + `Run` land beside its `Help/` tree
    // (`Apps/<name>.app/…`, `Services/<name>.app/…`), making every bundle a
    // complete, self-contained on-disk directory. The files are composed and
    // signed by the image pipeline's caller (this crate stays a pure
    // planter); the same discovered set feeds the QEMU fixture, so image and
    // fixture cannot drift.
    for (components, bytes) in apps {
        plant_nested_file(&mut fs, root, components, bytes)
            .map_err(MkimageError::SystemPartition)?;
    }
    fs.flush().map_err(MkimageError::SystemPartition)?;
    Ok(fs.into_block().into_bytes())
}

/// Lay the **writable-state** `/System` subtree under `system` on the
/// encrypted data root: the [`WRITABLE_SYSTEM_SUBDIRS`] (`Logs`, `Settings`,
/// and `Security` with its `Keys`/`Policy`), the seeded users database and
/// matching group registry under `Security` (every profile carries the
/// default system/service accounts), and — for a debug image — the baked
/// log-attestation key and machine-id. The immutable `/System` content is
/// **not** authored here (it lives on the read-only `ARXFSSystem` volume);
/// only what the writable root volume actually backs at runtime is laid
/// down.
fn populate_system_subtree(
    fs: &mut ARXFS<MemBlock>,
    system: NodeId,
    seed: &RootSeed<'_>,
) -> Result<(), MkimageError> {
    for sub in WRITABLE_SYSTEM_SUBDIRS {
        let node = fs
            .create(system, sub.as_bytes(), NodeKind::Directory)
            .map_err(MkimageError::RootPartition)?;
        if sub == "Security" {
            create_security_subdirs(fs, node, MkimageError::RootPartition)?;
        }
    }
    let security = fs
        .lookup(system, b"Security")
        .map_err(MkimageError::RootPartition)?;
    write_security_file(fs, security, USERS_DB_NAME, seed.users_db)?;
    write_security_file(fs, security, GROUPS_DB_NAME, seed.groups_db)?;
    write_network_config(fs, system, seed.network_conf)?;
    write_library_config(fs, system, seed.library_conf)?;
    if let Some(key_bytes) = seed.log_attestation_key {
        let keys = fs
            .lookup(security, b"Keys")
            .map_err(MkimageError::RootPartition)?;
        write_key_file(fs, keys, LOG_ATTESTATION_KEY_NAME, key_bytes)?;
    }
    if let Some(id_bytes) = seed.machine_id {
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
    fs: &mut ARXFS<MemBlock>,
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
            tairix_abi::DriverError::DeviceFault,
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
    fs: &mut ARXFS<MemBlock>,
    system: NodeId,
    wrap: fn(tairix_abi::DriverError) -> MkimageError,
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

/// Create the fixed `Keys` and `Policy` subdirectories under a `Security` node.
/// The one definition both the read-only `/System` volume and the encrypted
/// root's writable-state subtree author their `Security/{Keys, Policy}`
/// through, so the two cannot drift; `wrap` tags the failure with the partition
/// the caller is authoring.
fn create_security_subdirs(
    fs: &mut ARXFS<MemBlock>,
    security: NodeId,
    wrap: fn(tairix_abi::DriverError) -> MkimageError,
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
    fs: &mut ARXFS<MemBlock>,
    security: tairix_abi::driver::filesystem::NodeId,
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
            tairix_abi::DriverError::DeviceFault,
        ));
    }
    Ok(())
}

/// Lay out the network-configuration store on the writable root: create
/// `/System/Settings/Network` and write `document` through the one
/// `tairix_netconfig` engine.
///
/// The document is **parsed and re-rendered** rather than copied: the engine
/// that validates it here is the same one `netstack` reads it with, so an
/// image can never ship an addressing default its own stack would reject
/// (fail closed at build time, not at first boot). An unparseable document is
/// a build failure, as is a short write — never a truncated store.
fn write_network_config(
    fs: &mut ARXFS<MemBlock>,
    system: NodeId,
    document: &str,
) -> Result<(), MkimageError> {
    let settings = fs
        .lookup(system, b"Settings")
        .map_err(MkimageError::RootPartition)?;
    let network = fs
        .create(
            settings,
            NETWORK_SETTINGS_DIR.as_bytes(),
            NodeKind::Directory,
        )
        .map_err(MkimageError::RootPartition)?;
    let text = tairix_netconfig::NetworkConfig::parse(document)
        .map_err(|_| MkimageError::NetworkConfig)?
        .render();
    fs.create(network, NETWORK_CONF_NAME.as_bytes(), NodeKind::RegularFile)
        .map_err(MkimageError::RootPartition)?;
    let written = fs
        .write_at(network, NETWORK_CONF_NAME.as_bytes(), 0, text.as_bytes())
        .map_err(MkimageError::RootPartition)?;
    if written != text.len() {
        return Err(MkimageError::RootPartition(
            tairix_abi::DriverError::DeviceFault,
        ));
    }
    Ok(())
}

/// Lay out the program-library store on the writable root: create
/// `/System/Settings/ProgramLibrary` and write the machine-wide catalog
/// document the desktop's Program Library reads.
///
/// The document is derived from the planted bundles' own signed manifests
/// ([`crate::library::library_catalog`]) — an image with no listed
/// application ships the canonical **empty** store, which readers treat as
/// "no catalogued applications". The file keeps the authored default
/// security record (system-user-owned), so an ordinary account reads the
/// catalog but only the system identity rewrites it; a user personalises
/// through their own overlay instead. A short write is a build failure,
/// never a truncated store.
fn write_library_config(
    fs: &mut ARXFS<MemBlock>,
    system: NodeId,
    text: &str,
) -> Result<(), MkimageError> {
    let settings = fs
        .lookup(system, b"Settings")
        .map_err(MkimageError::RootPartition)?;
    let library = fs
        .create(
            settings,
            tairix_proglib::LIBRARY_SETTINGS_SUBDIR.as_bytes(),
            NodeKind::Directory,
        )
        .map_err(MkimageError::RootPartition)?;
    fs.create(
        library,
        tairix_proglib::LIBRARY_FILE.as_bytes(),
        NodeKind::RegularFile,
    )
    .map_err(MkimageError::RootPartition)?;
    let written = fs
        .write_at(
            library,
            tairix_proglib::LIBRARY_FILE.as_bytes(),
            0,
            text.as_bytes(),
        )
        .map_err(MkimageError::RootPartition)?;
    if written != text.len() {
        return Err(MkimageError::RootPartition(
            tairix_abi::DriverError::DeviceFault,
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
    fs: &mut ARXFS<MemBlock>,
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
            tairix_abi::DriverError::DeviceFault,
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
    use tairix_abi::DriverError;

    const TEST_SECTORS: u64 = 131_072; // 64 MiB, the production root size.
    const TEST_KEY: VolumeKey = [0x42; tairix_drv_fs_arxfs::VOLUME_KEY_LEN];

    /// Stand-in database texts: the seeding contract is "the given text is
    /// written verbatim", so the tests only need recognisable bytes (the
    /// real default set is pinned by `tairix_users::provision`'s own tests
    /// and by the callers in `crate::lib`).
    const TEST_USERS: &str = "tairix-users-v1\n# seeded for the test\n";
    const TEST_GROUPS: &str = "tairix-groups-v1\nwheel:1000\n";

    /// A debug-shaped home-directory spec: the seeded interactive account's
    /// `/Users/root`, owned by the first user-band uid/gid.
    const TEST_HOMES: &[(&str, u32, u32)] = &[("root", 1000, 1000)];

    /// A recognisable catalog document for the seeding contract ("the given
    /// text is written verbatim"); the real derivation is pinned by
    /// `crate::library`'s own tests.
    const TEST_LIBRARY: &str =
        "editor.name = Editor\neditor.bundle = /Apps/Editor.app\neditor.category = Office\n";

    /// A recognisable managed-interface document for the seeding contract:
    /// the writer must parse and re-render it rather than copy it blindly, so
    /// a malformed default fails the build.
    const TEST_NETWORK: &str =
        "wan.kind ethernet\nwan.match.node 0xfd580000\nwan.ipv4.method dhcp\n";

    /// The debug-shaped seed most tests build with: databases + home, no
    /// baked key or machine-id.
    const TEST_SEED: RootSeed<'static> = RootSeed {
        users_db: TEST_USERS,
        groups_db: TEST_GROUPS,
        home_dirs: TEST_HOMES,
        log_attestation_key: None,
        machine_id: None,
        library_conf: TEST_LIBRARY,
        network_conf: TEST_NETWORK,
    };

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
        build_root_partition(TEST_SECTORS, &TEST_KEY, &mut TestEntropy(7), &TEST_SEED)
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
        let mut fs = ARXFS::open(dev, &TEST_KEY).expect("the volume mounts under its key");
        let root = fs.root();
        for name in TOP_LEVEL_DIRS {
            fs.lookup(root, name.as_bytes())
                .unwrap_or_else(|_| panic!("/{name} exists"));
        }
        let system = fs.lookup(root, b"System").expect("/System exists");
        // The encrypted root carries ONLY the writable-state /System subtree;
        // the immutable content lives on the read-only ARXFSSystem volume.
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
        // the read-only ARXFSSystem volume mounted over `/`.
        for immutable in ["Kernel", "Drivers", "Libraries", "Fonts", "Services"] {
            assert!(
                fs.lookup(system, immutable.as_bytes()).is_err(),
                "/System/{immutable} must NOT exist on the writable root volume"
            );
        }
    }

    #[test]
    fn the_writable_root_ships_the_seeded_network_config() {
        let bytes = build();
        let dev = MemBlock::from_bytes(bytes).expect("whole sectors");
        let mut fs = ARXFS::open(dev, &TEST_KEY).expect("mounts");
        let root = fs.root();
        let system = fs.lookup(root, b"System").expect("/System exists");
        let settings = fs.lookup(system, b"Settings").expect("Settings exists");
        let network = fs
            .lookup(settings, NETWORK_SETTINGS_DIR.as_bytes())
            .expect("Settings/Network exists");
        let conf = fs
            .lookup(network, NETWORK_CONF_NAME.as_bytes())
            .expect("network.conf exists");
        // The seeded document lands, canonically rendered, and parses back to
        // exactly the interface the image builder asked for. The buffer is
        // sized well past the document so a truncated read can never make
        // this assertion pass vacuously.
        let mut buf = [0u8; tairix_netconfig::MAX_CONFIG_LEN];
        let read = fs.read_at(conf, 0, &mut buf).expect("network.conf reads");
        let text = core::str::from_utf8(&buf[..read]).expect("utf-8");
        let parsed = tairix_netconfig::NetworkConfig::parse(text).expect("parses");
        let expected =
            tairix_netconfig::NetworkConfig::parse(TEST_NETWORK).expect("the seed parses");
        assert_eq!(
            text,
            expected.render(),
            "the seeded document lands verbatim"
        );
        let wan = parsed.interface("wan").expect("the seeded interface");
        assert_eq!(wan.match_node, Some(0xfd58_0000));
        assert_eq!(wan.ipv4_method(), tairix_netconfig::Ipv4Method::Dhcp);
    }

    #[test]
    fn an_unparseable_network_config_fails_the_build() {
        // A malformed addressing default must fail the *image build*, not the
        // boot: the writer validates through the same engine `netstack` reads
        // the store with, so a document the stack would reject never ships.
        let seed = RootSeed {
            network_conf: "wan.kind ethernet\nwan.ipv4.method wireless\n",
            ..TEST_SEED
        };
        assert!(matches!(
            build_root_partition(TEST_SECTORS, &TEST_KEY, &mut TestEntropy(7), &seed),
            Err(MkimageError::NetworkConfig)
        ));
    }

    #[test]
    fn the_writable_root_ships_the_seeded_program_library() {
        let bytes = build();
        let dev = MemBlock::from_bytes(bytes).expect("whole sectors");
        let mut fs = ARXFS::open(dev, &TEST_KEY).expect("mounts");
        let root = fs.root();
        let system = fs.lookup(root, b"System").expect("/System exists");
        let settings = fs.lookup(system, b"Settings").expect("Settings exists");
        let library = fs
            .lookup(settings, tairix_proglib::LIBRARY_SETTINGS_SUBDIR.as_bytes())
            .expect("Settings/ProgramLibrary exists");
        let conf = fs
            .lookup(library, tairix_proglib::LIBRARY_FILE.as_bytes())
            .expect("library.conf exists");
        // The seeded document is written verbatim and parses through the
        // one catalog engine the desktop reads it with.
        let mut buf = [0u8; 256];
        let read = fs.read_at(conf, 0, &mut buf).expect("library.conf reads");
        let text = core::str::from_utf8(&buf[..read]).expect("utf-8");
        assert_eq!(text, TEST_LIBRARY);
        let document = tairix_appconf::Document::parse(text).expect("a well-formed document");
        let parsed = tairix_proglib::load(&document).expect("reads");
        assert_eq!(parsed.len(), 1);
    }

    #[test]
    fn the_wrong_key_is_refused() {
        let bytes = build();
        let dev = MemBlock::from_bytes(bytes).expect("whole sectors");
        let wrong: VolumeKey = [0x43; tairix_drv_fs_arxfs::VOLUME_KEY_LEN];
        assert_eq!(
            ARXFS::open(dev, &wrong).err(),
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
        assert!(build_root_partition(TEST_SECTORS, &TEST_KEY, &mut NoEntropy, &TEST_SEED).is_err());
    }

    #[test]
    fn a_seeded_users_database_is_written_and_reads_back() {
        let text = TEST_USERS;
        let bytes = build();
        let dev = MemBlock::from_bytes(bytes).expect("whole sectors");
        let mut fs = ARXFS::open(dev, &TEST_KEY).expect("mounts");
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
        let text = TEST_GROUPS;
        let bytes = build();
        let dev = MemBlock::from_bytes(bytes).expect("whole sectors");
        let mut fs = ARXFS::open(dev, &TEST_KEY).expect("mounts");
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
    fn a_seeded_home_directory_is_account_owned_and_owner_only() {
        use tairix_abi::driver::filesystem::FilesystemSecurity;

        let bytes = build();
        let dev = MemBlock::from_bytes(bytes).expect("whole sectors");
        let mut fs = ARXFS::open(dev, &TEST_KEY).expect("mounts");
        let root = fs.root();
        let users = fs.lookup(root, b"Users").expect("/Users exists");
        let home = fs.lookup(users, b"root").expect("/Users/root exists");
        // Owned by the seeded account, owner-only: the account enters and
        // writes its own home; no other ordinary principal reads it.
        let sec = fs.security(home).expect("security present");
        assert_eq!(sec.mode, HOME_MODE);
        assert_eq!(sec.uid, TEST_HOMES[0].1);
        assert_eq!(sec.gid, TEST_HOMES[0].2);
    }

    /// A seeded home carries the same fixed shape a provisioned one does,
    /// so the first per-user write on a debug image lands instead of
    /// failing on a missing ancestor.
    #[test]
    fn a_seeded_home_directory_carries_the_fixed_home_shape() {
        use tairix_abi::driver::filesystem::FilesystemSecurity;

        let bytes = build();
        let dev = MemBlock::from_bytes(bytes).expect("whole sectors");
        let mut fs = ARXFS::open(dev, &TEST_KEY).expect("mounts");
        let root = fs.root();
        let users = fs.lookup(root, b"Users").expect("/Users exists");
        let home = fs.lookup(users, b"root").expect("/Users/root exists");
        for name in HOME_SUBDIRS {
            let node = fs
                .lookup(home, name.as_bytes())
                .unwrap_or_else(|_| panic!("{name} exists in a seeded home"));
            let sec = fs.security(node).expect("security present");
            assert_eq!(sec.mode, HOME_MODE, "{name} is owner-only");
            assert_eq!(sec.uid, TEST_HOMES[0].1, "{name} belongs to the account");
            assert_eq!(sec.gid, TEST_HOMES[0].2, "{name} carries its group");
        }
    }

    /// The gated per-app data roots ship with the home, owned by the app-data
    /// service and capability-gated, with the search-only transit grant on
    /// every directory the service must descend through to reach them.
    ///
    /// A seeded image is the shape every other provisioner is compared
    /// against, so this is where a drift would first show.
    #[test]
    fn a_seeded_home_carries_the_gated_per_app_data_roots() {
        use tairix_abi::driver::filesystem::FilesystemSecurity;

        let bytes = build();
        let dev = MemBlock::from_bytes(bytes).expect("whole sectors");
        let mut fs = ARXFS::open(dev, &TEST_KEY).expect("mounts");
        let root = fs.root();
        let users = fs.lookup(root, b"Users").expect("/Users exists");
        let home = fs.lookup(users, b"root").expect("/Users/root exists");
        let expected_root = appdata_root_security();
        let expected_transit =
            appdata_transit_security(TEST_HOMES[0].1, TEST_HOMES[0].2).expect("one entry fits");

        // The service can descend into the home itself.
        assert_eq!(
            fs.security(home).expect("security present"),
            expected_transit
        );
        for parent in APPDATA_ROOT_PARENTS {
            let node = fs
                .lookup(home, parent.as_bytes())
                .unwrap_or_else(|_| panic!("{parent} exists"));
            assert_eq!(
                fs.security(node).expect("security present"),
                expected_transit,
                "{parent} lets the app-data service through and no further"
            );
            let gated = fs
                .lookup(node, APPDATA_ROOT.as_bytes())
                .unwrap_or_else(|_| panic!("{parent}/{APPDATA_ROOT} exists"));
            assert_eq!(
                fs.security(gated).expect("security present"),
                expected_root,
                "{parent}/{APPDATA_ROOT} is the gate"
            );
        }
    }

    #[test]
    fn an_installer_shaped_root_ships_no_log_attestation_key() {
        let bytes = build();
        let dev = MemBlock::from_bytes(bytes).expect("whole sectors");
        let mut fs = ARXFS::open(dev, &TEST_KEY).expect("mounts");
        let root = fs.root();
        let system = fs.lookup(root, b"System").expect("/System exists");
        let security = fs.lookup(system, b"Security").expect("Security exists");
        // The databases are always seeded; the log-attestation key never is
        // on an installer-shaped root — the first-boot installer generates
        // the per-installation key, never the image.
        fs.lookup(security, USERS_DB_NAME.as_bytes())
            .expect("Users database exists");
        fs.lookup(security, GROUPS_DB_NAME.as_bytes())
            .expect("Groups registry exists");
        let keys = fs.lookup(security, b"Keys").expect("Keys exists");
        assert!(fs
            .lookup(keys, LOG_ATTESTATION_KEY_NAME.as_bytes())
            .is_err());
    }

    #[test]
    fn a_seeded_log_attestation_key_is_written_locked_down_and_parses() {
        use tairix_abi::driver::filesystem::FilesystemSecurity;
        use tairix_log::{LogAttestationKey, LOG_ATTESTATION_KEY_FILE_LEN};

        // A debug-shaped key image: a real `LogAttestationKey` on-disk blob.
        let key_file = LogAttestationKey::from_key([0x5A; tairix_log::LOG_ATTESTATION_KEY_LEN])
            .to_file_bytes()
            .to_vec();
        let bytes = build_root_partition(
            TEST_SECTORS,
            &TEST_KEY,
            &mut TestEntropy(7),
            &RootSeed {
                log_attestation_key: Some(&key_file),
                ..TEST_SEED
            },
        )
        .expect("root partition builds");
        let dev = MemBlock::from_bytes(bytes).expect("whole sectors");
        let mut fs = ARXFS::open(dev, &TEST_KEY).expect("mounts");
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
        use tairix_abi::driver::filesystem::FilesystemSecurity;
        use tairix_abi::MACHINE_ID_LEN;

        let id = [0xA7u8; MACHINE_ID_LEN];
        let bytes = build_root_partition(
            TEST_SECTORS,
            &TEST_KEY,
            &mut TestEntropy(7),
            &RootSeed {
                machine_id: Some(&id),
                ..TEST_SEED
            },
        )
        .expect("root partition builds");
        let dev = MemBlock::from_bytes(bytes).expect("whole sectors");
        let mut fs = ARXFS::open(dev, &TEST_KEY).expect("mounts");
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
        let mut fs = ARXFS::open(dev, &TEST_KEY).expect("mounts");
        let root = fs.root();
        let system = fs.lookup(root, b"System").expect("/System exists");
        let security = fs.lookup(system, b"Security").expect("Security exists");
        // An installer-shaped root mints its machine-id at first boot, never
        // in the image.
        assert!(fs.lookup(security, MACHINE_ID_NAME.as_bytes()).is_err());
    }
}
