//! Single-source-of-truth rustfs disk-image fixture shared by the
//! end-to-end QEMU rustfs-over-virtio_blk vertical.
//!
//! Unlike the hand-built FAT32 fixture, this image is laid down by the
//! **real** rustfs driver: [`build_image`] formats an in-memory volume
//! through [`RustFs::format`](rustos_drv_fs_rustfs::RustFs::format), plants
//! [`PLANTED_FILE_NAME`] / [`PLANTED_FILE_CONTENT`] through the driver's
//! own write path, and returns the resulting bytes. The on-disk layout
//! therefore has exactly one author — the driver — so the fixture and the
//! driver can never drift (`AGENTS.md` §2.2).
//!
//! The host harness (`tools/xtask`) plants those bytes on the test's
//! backing disk before the guest boots. The freestanding guest tail
//! (`tests/integration/virtio_qemu_support`) mounts that very volume
//! through the real rustfs driver, verifies the planted file, then
//! creates and writes a fresh file and reads it back. Both sides name the
//! same fixed files through the constants below, so the on-disk contract
//! lives in exactly one place.
//!
//! The image is a genuine rustfs volume — 1 MiB, 512-byte blocks, 64
//! inodes — laid out so the real `RustFs::open` validator accepts it. It
//! is `no_std` + `alloc` so it links into both the host build tool and
//! the freestanding guest test.

#![no_std]
#![forbid(unsafe_code)]
#![deny(missing_docs)]

extern crate alloc;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use rustos_abi::driver::block::{Block, BlockGeometry};
use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeId, NodeKind};
use rustos_abi::DriverError;
use rustos_caps::CapabilitySet;
use rustos_drv_fs_rustfs::{EntropySource, RustFs, VolumeKey, VOLUME_KEY_LEN};
use rustos_users::{AccountState, Gid, Identity, ParseError, Salt, Uid, UserRecord, UsersDb};

/// Logical block (sector) size of the produced image, in bytes. Matches
/// both the 512-byte sector QEMU's virtio-blk reports by default and the
/// rustfs minimum block size, so the volume the driver formats here maps
/// directly onto the device the guest mounts.
pub const SECTOR_BYTES: usize = 512;

/// Total size of the produced image, in 512-byte sectors (1 MiB). Large
/// enough for the inode table, bitmap, journal, and a non-trivial data
/// region, matching the FAT32 fixture's footprint.
pub const TOTAL_SECTORS: u64 = 2048;

/// Number of inodes the volume is formatted with. Two-per-block at the
/// 512-byte block size, comfortably more than the root plus the planted
/// and written files need.
const INODE_COUNT: u32 = 64;

/// Volume key the fixture is formatted and mounted with. `RustFS` is
/// encrypted-by-default with no plaintext layout
/// (`docs/src/filesystem/rustfs-spec.md` §5), so the host builder and the
/// guest tail mount the planted volume under this single shared key.
pub const FIXTURE_VOLUME_KEY: VolumeKey = [0x5a; VOLUME_KEY_LEN];

/// Deterministic stand-in for the platform RNG seam used to provision the
/// fixture volume. A fixed sequence keeps the built image **reproducible**
/// (`AGENTS.md` §19.3); it is fixture scaffolding, never a production source.
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

/// File planted in the root directory before boot. The guest tail looks
/// it up and verifies [`PLANTED_FILE_CONTENT`].
pub const PLANTED_FILE_NAME: &[u8] = b"hello.txt";

/// Contents of [`PLANTED_FILE_NAME`].
pub const PLANTED_FILE_CONTENT: &[u8] = b"Hello from a planted rustfs volume on virtio-blk.\n";

/// File the guest tail creates and writes after mounting.
pub const NEW_FILE_NAME: &[u8] = b"written.txt";

/// Contents the guest tail writes to [`NEW_FILE_NAME`] and reads back.
pub const NEW_FILE_CONTENT: &[u8] = b"RustOS wrote this file to rustfs over virtio-blk.\n";

/// Username of the single account planted on the users-root volume
/// ([`build_users_root_image`]).
pub const USERS_FIXTURE_USERNAME: &str = "root";

/// Password of the planted [`USERS_FIXTURE_USERNAME`] account.
pub const USERS_FIXTURE_PASSWORD: &str = "root";

/// PBKDF2 cost of the planted account's password record: the format's
/// floor, so the guest-side authentication proof stays fast under QEMU
/// TCG. Fixture scaffolding only — a real database uses
/// [`rustos_users::DEFAULT_ITERATIONS`].
pub const USERS_FIXTURE_ITERATIONS: u32 = rustos_users::MIN_ITERATIONS;

/// Fixed salt of the planted account's password record, keeping the
/// built image reproducible (`AGENTS.md` §19.3).
const USERS_FIXTURE_SALT: Salt = [0xa5; rustos_users::SALT_LEN];

/// Serialise the users-root volume's `/System/Security/Users` database:
/// the single active [`USERS_FIXTURE_USERNAME`] account with an empty
/// capability ceiling.
///
/// # Errors
///
/// Propagates the [`ParseError`] if a fixture constant violates the
/// `users-v1` bounds — a programming error in this fixture, surfaced
/// rather than panicked (`AGENTS.md` §2.9).
pub fn users_db_text() -> Result<String, ParseError> {
    let record = UserRecord::with_password(
        Identity {
            username: USERS_FIXTURE_USERNAME,
            uid: Uid(0),
            primary_gid: Gid(0),
            supplementary_gids: &[],
            display_name: "System Administrator",
            home: "/Users/root",
            shell: "/Apps/Shell.app/Run",
            capabilities: CapabilitySet::empty(),
            state: AccountState::Active,
        },
        USERS_FIXTURE_PASSWORD.as_bytes(),
        USERS_FIXTURE_SALT,
        USERS_FIXTURE_ITERATIONS,
    )?;
    Ok(UsersDb::new(alloc::vec![record])?.serialise())
}

/// In-memory [`Block`] device backing the fixture build and the host
/// round-trip tests. It addresses [`SECTOR_BYTES`]-byte sectors exactly
/// as the guest's virtio-blk device does.
pub struct VecBlock {
    store: Vec<u8>,
}

impl VecBlock {
    /// A zeroed device of `sectors` sectors.
    fn new(sectors: u64) -> Self {
        let len = usize::try_from(sectors).unwrap_or(0) * SECTOR_BYTES;
        Self {
            store: vec![0u8; len],
        }
    }

    /// Wrap an already-laid-out image (e.g. the bytes returned by
    /// [`build_users_root_image_with_key`]) as a mountable device, so a
    /// consumer can re-open it through the real `RustFs::open` without
    /// re-deriving the on-disk layout (`AGENTS.md` §2.2 — one block-device
    /// double, shared by the fixture and its consumers).
    #[must_use]
    pub fn from_bytes(bytes: Vec<u8>) -> Self {
        Self { store: bytes }
    }

    /// Byte span `[start, end)` for `len` bytes at sector `lba`, or an
    /// error if the access is unaligned or out of range.
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

impl Block for VecBlock {
    fn geometry(&self) -> Result<BlockGeometry, DriverError> {
        Ok(BlockGeometry {
            block_size: u32::try_from(SECTOR_BYTES).unwrap_or(0),
            block_count: TOTAL_SECTORS,
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

/// Build the rustfs image described in the module docs by driving the
/// real rustfs driver: format a fresh in-memory volume, plant
/// [`PLANTED_FILE_NAME`] with [`PLANTED_FILE_CONTENT`], flush, and return
/// the resulting on-disk bytes.
///
/// # Errors
///
/// Propagates any [`DriverError`] from the driver. The fixed geometry and
/// payload sizes make a failure a programming error in this fixture, but
/// the result is surfaced rather than panicked so the builder holds to
/// `AGENTS.md` §2.9 in every path it links into.
pub fn build_image() -> Result<Vec<u8>, DriverError> {
    let dev = VecBlock::new(TOTAL_SECTORS);
    let mut fs = RustFs::format(
        dev,
        INODE_COUNT,
        &FIXTURE_VOLUME_KEY,
        &mut FixtureEntropy { next: 1 },
    )?;
    let root = fs.root();
    fs.create(root, PLANTED_FILE_NAME, NodeKind::RegularFile)?;
    let written = fs.write_at(root, PLANTED_FILE_NAME, 0, PLANTED_FILE_CONTENT)?;
    if written != PLANTED_FILE_CONTENT.len() {
        return Err(DriverError::DeviceFault);
    }
    fs.flush()?;
    Ok(fs.into_block().into_bytes())
}

/// Build the users-root volume: a rustfs image carrying the `AGENTS.md`
/// §16.1 top-level directories with `/System/Security/Users` holding the
/// [`users_db_text`] database — the on-disk shape the production root
/// volume gives the kernel's boot-time users-database load
/// (`rustos_kernel_core::users`, `plans/PI.md` P11).
///
/// The volume is keyed by the same [`FIXTURE_VOLUME_KEY`] and geometry as
/// [`build_image`]; only the planted tree differs.
///
/// # Errors
///
/// Propagates any [`DriverError`] from the driver; a fixture users
/// database that violates the `users-v1` bounds surfaces as
/// [`DriverError::Unsupported`] (a programming error in this fixture,
/// surfaced rather than panicked — `AGENTS.md` §2.9).
pub fn build_users_root_image() -> Result<Vec<u8>, DriverError> {
    build_users_root_image_with_key(&FIXTURE_VOLUME_KEY)
}

/// Build the users-root volume under an arbitrary `volume_key` — the same
/// layout as [`build_users_root_image`] but keyed by the caller's key, so
/// a consumer can exercise the production passphrase-derived-key mount
/// path (`plans/PI.md` P11 root-mount; `kernel/rustos-kernel::root_mount`)
/// against a real on-disk volume. [`build_users_root_image`] delegates
/// here with [`FIXTURE_VOLUME_KEY`] (`AGENTS.md` §2.2 — one authoring
/// path).
///
/// # Errors
///
/// Propagates any [`DriverError`] from the driver; a fixture users
/// database that violates the `users-v1` bounds surfaces as
/// [`DriverError::Unsupported`] (a programming error in this fixture,
/// surfaced rather than panicked — `AGENTS.md` §2.9).
pub fn build_users_root_image_with_key(volume_key: &VolumeKey) -> Result<Vec<u8>, DriverError> {
    build_users_root_image_with_key_and_drivers(volume_key, &[])
}

/// Build the users-root volume under `volume_key`, additionally planting a
/// set of installed driver bundles into the `/System/Drivers/` store — the
/// on-disk shape a real installation gives the §18.3 / §18.6 autoload scan
/// (`rustos_kernel::driver_autoload::autoload_from_mounted_root`,
/// `plans/PI.md` P10 5d-2-ii).
///
/// Each driver is `(path_components, bytes)` where `path_components` is the
/// path *under the volume root* of the bundle's leaf file (for example
/// `&[b"System", b"Drivers", b"input", b"virtio_kbd", b"Run"]`). Intermediate
/// directories are created on demand, so several bundles can share a parent
/// (`System` is already created for the users database). The bytes are the
/// signed `.rxe` bundle exactly as the store scanner reads it back — the one
/// on-disk authoring path the QEMU autoload vertical and any future image
/// builder share (`AGENTS.md` §2.2). [`build_users_root_image_with_key`]
/// delegates here with no drivers.
///
/// # Errors
///
/// Propagates any [`DriverError`] from the driver; a fixture users database
/// that violates the `users-v1` bounds surfaces as
/// [`DriverError::Unsupported`], and a short write of a planted file as
/// [`DriverError::DeviceFault`] (a programming error in the fixture,
/// surfaced rather than panicked — `AGENTS.md` §2.9).
pub fn build_users_root_image_with_key_and_drivers(
    volume_key: &VolumeKey,
    drivers: &[(&[&[u8]], &[u8])],
) -> Result<Vec<u8>, DriverError> {
    let text = users_db_text().map_err(|_| DriverError::Unsupported)?;
    let dev = VecBlock::new(TOTAL_SECTORS);
    let mut fs = RustFs::format(
        dev,
        INODE_COUNT,
        volume_key,
        &mut FixtureEntropy { next: 1 },
    )?;
    let root = fs.root();
    for name in ["System", "Users", "Apps", "Storage"] {
        let node = fs.create(root, name.as_bytes(), NodeKind::Directory)?;
        if name == "System" {
            let security = fs.create(node, b"Security", NodeKind::Directory)?;
            fs.create(security, b"Users", NodeKind::RegularFile)?;
            let written = fs.write_at(security, b"Users", 0, text.as_bytes())?;
            if written != text.len() {
                return Err(DriverError::DeviceFault);
            }
        }
    }
    for (components, bytes) in drivers {
        plant_nested_file(&mut fs, root, components, bytes)?;
    }
    fs.flush()?;
    Ok(fs.into_block().into_bytes())
}

/// Plant a regular file at `components` (a path of directory names ending in
/// the file name) under `parent`, creating each intermediate directory that
/// does not already exist.
///
/// Used to lay driver bundles into `/System/Drivers/` without re-deriving the
/// directory walk per bundle (`AGENTS.md` §2.2).
///
/// # Errors
///
/// Propagates any [`DriverError`] from the driver, or [`DriverError::Unsupported`]
/// for an empty `components` path. A short write surfaces as
/// [`DriverError::DeviceFault`].
fn plant_nested_file<B>(
    fs: &mut RustFs<B>,
    parent: NodeId,
    components: &[&[u8]],
    bytes: &[u8],
) -> Result<(), DriverError>
where
    B: Block,
{
    let (file_name, dirs) = components.split_last().ok_or(DriverError::Unsupported)?;
    let mut node = parent;
    for dir in dirs {
        node = match fs.lookup(node, dir) {
            Ok(existing) => existing,
            Err(_) => fs.create(node, dir, NodeKind::Directory)?,
        };
    }
    fs.create(node, file_name, NodeKind::RegularFile)?;
    let written = fs.write_at(node, file_name, 0, bytes)?;
    if written != bytes.len() {
        return Err(DriverError::DeviceFault);
    }
    Ok(())
}

impl VecBlock {
    /// Consume the device, yielding its raw image bytes.
    fn into_bytes(self) -> Vec<u8> {
        self.store
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mount() -> RustFs<VecBlock> {
        let bytes = build_image().expect("the fixture builds a valid rustfs volume");
        let dev = VecBlock { store: bytes };
        RustFs::open(dev, &FIXTURE_VOLUME_KEY).expect("the built image is a valid rustfs volume")
    }

    #[test]
    fn image_is_exactly_the_advertised_size() {
        let bytes = build_image().expect("build image");
        let expected =
            usize::try_from(TOTAL_SECTORS).expect("sector count fits usize") * SECTOR_BYTES;
        assert_eq!(bytes.len(), expected);
    }

    #[test]
    fn driver_mounts_the_built_image() {
        let _fs = mount();
    }

    #[test]
    fn planted_file_reads_back_its_known_contents() {
        let mut fs = mount();
        let root = fs.root();
        let node = fs.lookup(root, PLANTED_FILE_NAME).expect("planted present");
        let mut buf = [0u8; 128];
        let n = fs.read_at(node, 0, &mut buf).expect("read planted file");
        assert_eq!(&buf[..n], PLANTED_FILE_CONTENT);
    }

    #[test]
    fn a_fresh_file_round_trips_through_create_write_and_read() {
        let mut fs = mount();
        let root = fs.root();
        fs.create(root, NEW_FILE_NAME, NodeKind::RegularFile)
            .expect("create new file");
        let written = fs
            .write_at(root, NEW_FILE_NAME, 0, NEW_FILE_CONTENT)
            .expect("write new file");
        assert_eq!(written, NEW_FILE_CONTENT.len());

        let node = fs.lookup(root, NEW_FILE_NAME).expect("new file present");
        let mut buf = [0u8; 128];
        let n = fs.read_at(node, 0, &mut buf).expect("read new file");
        assert_eq!(&buf[..n], NEW_FILE_CONTENT);
    }

    /// No-op audit sink: the round-trip test asserts behaviour through
    /// the returned database, not the audit stream (the audit records
    /// are covered by kernel/core's own loader tests).
    struct DiscardSink;

    impl rustos_log::Sink for DiscardSink {
        fn write_event(&self, _event: &rustos_log::Event<'_>) {}
    }

    #[test]
    fn users_root_image_mounts_and_the_kernel_loader_reads_the_database() {
        let bytes = build_users_root_image().expect("users-root image builds");
        let dev = VecBlock { store: bytes };
        let mut fs =
            RustFs::open(dev, &FIXTURE_VOLUME_KEY).expect("users-root image is a valid volume");

        let sink = DiscardSink;
        let db = rustos_kernel_core::load_users_db(&mut fs, &sink)
            .expect("the kernel loader reads /System/Security/Users");
        assert_eq!(db.records().len(), 1);

        let record = db
            .authenticate(USERS_FIXTURE_USERNAME, USERS_FIXTURE_PASSWORD.as_bytes())
            .expect("the planted account authenticates");
        assert_eq!(record.username(), USERS_FIXTURE_USERNAME);

        db.authenticate(USERS_FIXTURE_USERNAME, b"wrong password")
            .expect_err("a wrong password is refused");
    }

    #[test]
    fn a_planted_driver_bundle_reads_back_from_the_system_drivers_store() {
        // The §18.6 store-planting path: a driver bundle laid into
        // `/System/Drivers/input/virtio_kbd/Run` is created (with every
        // intermediate directory) and reads back byte-for-byte off the
        // mounted volume — the on-disk shape the autoload store scan walks
        // (`AGENTS.md` §16.2 / §18.3).
        let bundle: &[u8] = b"a-signed-rxe-bundle-stand-in";
        let path: &[&[u8]] = &[b"System", b"Drivers", b"input", b"virtio_kbd", b"Run"];
        let bytes =
            build_users_root_image_with_key_and_drivers(&FIXTURE_VOLUME_KEY, &[(path, bundle)])
                .expect("users-root image with a planted driver builds");
        let dev = VecBlock { store: bytes };
        let mut fs = RustFs::open(dev, &FIXTURE_VOLUME_KEY).expect("the volume mounts");

        let mut node = fs.root();
        for dir in [b"System".as_slice(), b"Drivers", b"input", b"virtio_kbd"] {
            node = fs.lookup(node, dir).expect("store directory present");
        }
        let run = fs.lookup(node, b"Run").expect("the bundle leaf file");
        let mut buf = [0u8; 64];
        let n = fs.read_at(run, 0, &mut buf).expect("read the bundle bytes");
        assert_eq!(&buf[..n], bundle);

        // The users database the same volume carries still authenticates,
        // so planting a driver did not disturb the rest of the tree.
        let security = {
            let system = fs.lookup(fs.root(), b"System").expect("System present");
            fs.lookup(system, b"Security").expect("Security present")
        };
        let users = fs.lookup(security, b"Users").expect("Users present");
        let mut db_buf = [0u8; 512];
        let read = fs.read_at(users, 0, &mut db_buf).expect("read users db");
        assert!(read > 0, "the users database is still present");
    }

    #[test]
    fn the_planted_file_survives_a_second_file_being_written() {
        let mut fs = mount();
        let root = fs.root();
        fs.create(root, NEW_FILE_NAME, NodeKind::RegularFile)
            .expect("create new file");
        fs.write_at(root, NEW_FILE_NAME, 0, NEW_FILE_CONTENT)
            .expect("write new file");

        let node = fs.lookup(root, PLANTED_FILE_NAME).expect("planted present");
        let mut buf = [0u8; 128];
        let n = fs.read_at(node, 0, &mut buf).expect("read planted file");
        assert_eq!(&buf[..n], PLANTED_FILE_CONTENT);
    }
}
