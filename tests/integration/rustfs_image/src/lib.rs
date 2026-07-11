//! Single-source-of-truth rustfs disk-image fixture shared by the
//! end-to-end QEMU rustfs-over-virtio_blk vertical.
//!
//! Unlike the hand-built FAT32 fixture, this image is laid down by the
//! **real** rustfs driver: [`build_image`] formats an in-memory volume
//! through [`RustFs::format`](rustos_drv_fs_rustfs::RustFs::format), plants
//! [`PLANTED_FILE_NAME`] / [`PLANTED_FILE_CONTENT`] through the driver's
//! own write path, and returns the resulting bytes. The on-disk layout
//! therefore has exactly one author — the driver — so the fixture and the
//! driver can never drift.
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
use rustos_abi::driver::filesystem::{FilesystemRead, FilesystemWrite, NodeKind};
use rustos_abi::DriverError;
use rustos_drv_fs_rustfs::{EntropySource, RustFs, VolumeKey, VOLUME_KEY_LEN};
use rustos_users::{
    AccountState, Gid, GroupRecord, GroupsDb, Identity, ParseError, Salt, Uid, UserRecord, UsersDb,
};

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
/// fixture volume. A fixed sequence keeps the built image **reproducible**; it is fixture scaffolding, never a production source.
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
/// built image reproducible.
const USERS_FIXTURE_SALT: Salt = [0xa5; rustos_users::SALT_LEN];

/// Serialise the users-root volume's `/System/Security/Users` database:
/// the single active [`USERS_FIXTURE_USERNAME`] account granted the shared
/// administrator capability ceiling (`rustos_users::administrator_ceiling`
/// — the session baseline plus the administrative set), exactly as the
/// real debug image's `tools/mkimage::debug_users_db` seeds it, so the
/// end-to-end session vertical exercises the same grant the shipped debug
/// profile carries (`plans/CAPABILITY_USE.md` CU3).
///
/// # Errors
///
/// Propagates the [`ParseError`] if a fixture constant violates the
/// `users-v1` bounds — a programming error in this fixture, surfaced
/// rather than panicked.
pub fn users_db_text() -> Result<String, ParseError> {
    let record = UserRecord::with_password(
        Identity {
            username: USERS_FIXTURE_USERNAME,
            uid: Uid(0),
            primary_gid: Gid(0),
            supplementary_gids: &[rustos_users::STORAGE_GID],
            display_name: "System Administrator",
            home: "/Users/root",
            shell: "/System/Apps/elsh.app/Run",
            capabilities: rustos_users::administrator_ceiling(),
            state: AccountState::Active,
        },
        USERS_FIXTURE_PASSWORD.as_bytes(),
        USERS_FIXTURE_SALT,
        USERS_FIXTURE_ITERATIONS,
    )?;
    Ok(UsersDb::new(alloc::vec![record])?.serialise())
}

/// Serialise the users-root volume's `/System/Security/Groups` registry:
/// the `wheel` group (gid 0) the planted [`USERS_FIXTURE_USERNAME`]
/// account names as its primary group — so the kernel's boot-time identity
/// table build (`rustos_kernel_core::build_identity_table`) resolves the
/// account's gid against a real registry rather than failing closed on a
/// dangling reference — plus the well-known removable-storage group
/// ([`rustos_users::STORAGE_GROUP`]) the account is a member of, exactly
/// as `tools/mkimage::debug_groups_db` seeds the shipped debug profile,
/// so the unlock's storage-gid resolution is exercised end to end.
///
/// # Errors
///
/// Propagates the [`ParseError`] if a fixture constant violates the
/// `groups-v1` bounds — a programming error in this fixture, surfaced
/// rather than panicked.
pub fn groups_db_text() -> Result<String, ParseError> {
    let records = alloc::vec![
        GroupRecord::new("wheel", Gid(0))?,
        GroupRecord::new(rustos_users::STORAGE_GROUP, rustos_users::STORAGE_GID)?,
    ];
    Ok(GroupsDb::new(records)?.serialise())
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
    /// re-deriving the on-disk layout (one block-device
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
/// in every path it links into.
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

/// Build the users-root volume: a rustfs image carrying the top-level directories with `/System/Security/Users` holding the
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
/// surfaced rather than panicked).
pub fn build_users_root_image() -> Result<Vec<u8>, DriverError> {
    build_users_root_image_with_key(&FIXTURE_VOLUME_KEY)
}

/// Build the users-root volume under an arbitrary `volume_key` — the same
/// layout as [`build_users_root_image`] but keyed by the caller's key, so
/// a consumer can exercise the production passphrase-derived-key mount
/// path (`plans/PI.md` P11 root-mount; `kernel/rustos-kernel::root_mount`)
/// against a real on-disk volume. [`build_users_root_image`] delegates
/// here with [`FIXTURE_VOLUME_KEY`] (one authoring
/// path).
///
/// # Errors
///
/// Propagates any [`DriverError`] from the driver; a fixture users
/// database that violates the `users-v1` bounds surfaces as
/// [`DriverError::Unsupported`] (a programming error in this fixture,
/// surfaced rather than panicked).
pub fn build_users_root_image_with_key(volume_key: &VolumeKey) -> Result<Vec<u8>, DriverError> {
    let text = users_db_text().map_err(|_| DriverError::Unsupported)?;
    let groups_text = groups_db_text().map_err(|_| DriverError::Unsupported)?;
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
        if name == "Users" {
            // The planted account's recorded home directory, so a logged-in
            // session's `cd /Users/root` resolves against a real inode.
            fs.create(node, b"root", NodeKind::Directory)?;
        }
        if name == "System" {
            let security = fs.create(node, b"Security", NodeKind::Directory)?;
            fs.create(security, b"Users", NodeKind::RegularFile)?;
            let written = fs.write_at(security, b"Users", 0, text.as_bytes())?;
            if written != text.len() {
                return Err(DriverError::DeviceFault);
            }
            // The group registry the kernel identity-table build resolves
            // the planted account's primary gid against; without it the
            // build fails closed on the dangling gid 0 reference.
            fs.create(security, b"Groups", NodeKind::RegularFile)?;
            let written = fs.write_at(security, b"Groups", 0, groups_text.as_bytes())?;
            if written != groups_text.len() {
                return Err(DriverError::DeviceFault);
            }
        }
    }
    fs.flush()?;
    Ok(fs.into_block().into_bytes())
}

/// Re-export of the single store-planting helper. The
/// definition lives in the rustfs driver (`rustos_drv_fs_rustfs`) so the
/// image builder (`tools/mkimage`) and these fixtures share one routine that
/// gives the autoload scan an identical on-disk shape.
pub use rustos_drv_fs_rustfs::plant_nested_file;

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
        // The planted grant round-trips as exactly the shared administrator
        // ceiling — the same set the debug image seeds — so the end-to-end
        // session vertical exercises the real CU3 grant.
        assert_eq!(record.capabilities(), rustos_users::administrator_ceiling());

        db.authenticate(USERS_FIXTURE_USERNAME, b"wrong password")
            .expect_err("a wrong password is refused");

        // The account's recorded home directory exists on the volume.
        let users = fs.lookup(fs.root(), b"Users").expect("/Users present");
        fs.lookup(users, b"root").expect("/Users/root present");
    }

    #[test]
    fn plant_nested_file_lays_a_bundle_and_creates_intermediate_directories() {
        // The shared store-planting helper (`plant_nested_file`): a
        // driver bundle laid at the design-B `/System` volume's
        // `Drivers/input/virtio_kbd/Run` is created with every intermediate
        // directory and reads back byte-for-byte off the mounted volume — the
        // on-disk shape the autoload store scan walks. Driver bundles live on the `/System` volume under design B,
        // so the path is relative to that volume's root (no `System` prefix).
        let bundle: &[u8] = b"a-signed-rxe-bundle-stand-in";
        let path: &[&[u8]] = &[b"Drivers", b"input", b"virtio_kbd", b"Run"];

        let dev = VecBlock::new(TOTAL_SECTORS);
        let mut entropy = FixtureEntropy { next: 1 };
        let mut fs = RustFs::format(dev, INODE_COUNT, &FIXTURE_VOLUME_KEY, &mut entropy)
            .expect("a fresh volume formats");
        let root = fs.root();
        plant_nested_file(&mut fs, root, path, bundle).expect("the bundle plants");

        let mut node = fs.root();
        for dir in [b"Drivers".as_slice(), b"input", b"virtio_kbd"] {
            node = fs.lookup(node, dir).expect("store directory present");
        }
        let run = fs.lookup(node, b"Run").expect("the bundle leaf file");
        let mut buf = [0u8; 64];
        let n = fs.read_at(run, 0, &mut buf).expect("read the bundle bytes");
        assert_eq!(&buf[..n], bundle);
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
