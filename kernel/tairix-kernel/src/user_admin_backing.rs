//! The root-volume storage the `CAP_USER_ADMIN` account-administration
//! engine commits through (`plans/CAPABILITY_USE.md` CU4).
//!
//! `/System/Security` lives on the **writable encrypted root volume**,
//! which the VFS mount layout deliberately shadows with the read-only
//! `/System` volume — the account databases are reachable only through the
//! driver backing the root volume, exactly like the boot-time load.
//! The engine therefore shares the **one** registered root-volume driver
//! (the same `SleepLock`-serialised instance the `fs_*` syscalls run
//! against): the volume has a single writer, so its copy-on-write
//! allocation state can never diverge between two independent windows, and
//! every mutation is visible to the filesystem cache wrapped around that
//! driver.
//!
//! Persistence is crash-safe: each database is written whole to a
//! sibling temporary node carrying the original's security record, then
//! renamed over the original, so a power cut mid-write leaves either the
//! old or the new database — never a torn one. Home provisioning walks
//! and creates the directory path, then stamps the leaf owner-only under
//! the new account's identity; an already-present leaf is left untouched
//! (idempotent).

use alloc::sync::Arc;

use tairix_abi::driver::filesystem::{
    FilesystemRead, FilesystemSecurity, FilesystemWrite, NodeId, NodeKind, NodeSecurity,
};
use tairix_abi::{DriverError, Errno};
use tairix_kernel_core::{SleepLock, UserAdminBacking, VfsError};

/// Owner-only mode a freshly provisioned home directory is stamped with.
const HOME_MODE: u32 = 0o700;

/// The production [`UserAdminBacking`]: commits the engine's edits to the
/// mounted encrypted root volume.
pub struct RootAdminBacking<F: 'static> {
    /// The root volume's single registered driver, shared with the `fs_*`
    /// path (`LateFilesystem::register` returns this lock). A sleep lock,
    /// because a commit parks on device completion; holding it serialises
    /// an admin commit against every concurrent `fs_*` operation on the
    /// same volume.
    fs: Arc<SleepLock<F>>,
}

impl<F> RootAdminBacking<F> {
    /// Share the registered root-volume driver.
    #[must_use]
    pub const fn new(fs: Arc<SleepLock<F>>) -> Self {
        Self { fs }
    }
}

impl<F> UserAdminBacking for RootAdminBacking<F>
where
    F: FilesystemRead + FilesystemWrite + FilesystemSecurity + Send + 'static,
{
    fn persist(&self, users_text: &str, groups_text: &str) -> Result<(), Errno> {
        let mut fs = self.fs.lock();
        let fs = &mut *fs;
        let security_dir = resolve_security_dir(fs)?;
        replace_file(
            fs,
            security_dir,
            b"Users",
            b"Users.tmp",
            users_text.as_bytes(),
        )?;
        replace_file(
            fs,
            security_dir,
            b"Groups",
            b"Groups.tmp",
            groups_text.as_bytes(),
        )?;
        fs.flush().map_err(driver_errno)
    }

    fn provision_home(&self, home: &str, uid: u32, gid: u32) -> Result<(), Errno> {
        // The engine validated `home` as a bounded absolute path; re-check
        // the shape here so this window can never be driven relative
        // (fail closed).
        let mut components = home.split('/');
        if components.next() != Some("") {
            return Err(Errno::OutOfRange);
        }
        let mut fs = self.fs.lock();
        let fs = &mut *fs;
        let mut dir = fs.root();
        let mut walked = false;
        let mut remaining = components.peekable();
        while let Some(component) = remaining.next() {
            if component.is_empty() {
                return Err(Errno::OutOfRange);
            }
            walked = true;
            let is_leaf = remaining.peek().is_none();
            match fs.lookup(dir, component.as_bytes()) {
                Ok(node) => {
                    // An already-present leaf is left untouched: provisioning
                    // is idempotent and never rewrites ownership of existing
                    // data. An intermediate must be a directory.
                    let info = fs.node_info(node).map_err(driver_errno)?;
                    if info.kind != NodeKind::Directory {
                        return Err(Errno::AlreadyExists);
                    }
                    if is_leaf {
                        return Ok(());
                    }
                    dir = node;
                }
                Err(DriverError::NotFound) => {
                    let node = fs
                        .create(dir, component.as_bytes(), NodeKind::Directory)
                        .map_err(driver_errno)?;
                    if is_leaf {
                        fs.set_security(node, NodeSecurity::new(HOME_MODE, uid, gid))
                            .map_err(driver_errno)?;
                        return fs.flush().map_err(driver_errno);
                    }
                    dir = node;
                }
                Err(err) => return Err(driver_errno(err)),
            }
        }
        // A path with no components ("/") provisions nothing.
        if walked {
            Ok(())
        } else {
            Err(Errno::OutOfRange)
        }
    }
}

/// Resolve `/System/Security` on the root volume's own tree.
fn resolve_security_dir<F>(fs: &mut F) -> Result<NodeId, Errno>
where
    F: FilesystemRead + ?Sized,
{
    let system = fs.lookup(fs.root(), b"System").map_err(driver_errno)?;
    fs.lookup(system, b"Security").map_err(driver_errno)
}

/// Replace `dir/name` with `data`, crash-safely: the bytes are written
/// whole to `dir/tmp_name` (carrying the original's security record), then
/// renamed over the original. The target must already exist — the boot
/// image authors both databases, so a missing one is a provisioning defect
/// surfaced as [`Errno::NotFound`], never silently created with a guessed
/// security record.
fn replace_file<F>(
    fs: &mut F,
    dir: NodeId,
    name: &[u8],
    tmp_name: &[u8],
    data: &[u8],
) -> Result<(), Errno>
where
    F: FilesystemRead + FilesystemWrite + FilesystemSecurity + ?Sized,
{
    let existing = fs.lookup(dir, name).map_err(driver_errno)?;
    let security = fs.security(existing).map_err(driver_errno)?;

    // Clear any stale temporary from an interrupted earlier commit.
    match fs.remove(dir, tmp_name) {
        Ok(()) | Err(DriverError::NotFound) => {}
        Err(err) => return Err(driver_errno(err)),
    }

    let tmp = fs
        .create(dir, tmp_name, NodeKind::RegularFile)
        .map_err(driver_errno)?;
    let written = fs.write_at(dir, tmp_name, 0, data).map_err(driver_errno)?;
    if written != data.len() {
        // A short write never becomes the live database; drop the temp.
        let _ = fs.remove(dir, tmp_name);
        return Err(VfsError::Io.to_errno());
    }
    fs.set_security(tmp, security).map_err(driver_errno)?;
    // Land the temp durably before it replaces the original, so the rename
    // can never expose a half-written database.
    fs.flush().map_err(driver_errno)?;
    fs.rename(dir, tmp_name, dir, name).map_err(driver_errno)
}

/// Map a driver refusal onto the stable [`Errno`] the syscall reports.
fn driver_errno(err: DriverError) -> Errno {
    match err {
        DriverError::NotFound => Errno::NotFound,
        DriverError::PermissionDenied => Errno::PermissionDenied,
        DriverError::NoSpace => Errno::NoSpace,
        _ => VfsError::Io.to_errno(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::vec::Vec;

    use tairix_abi::driver::block::Block;
    use tairix_drv_fs_arxfs::{EntropySource, VolumeKey, ARXFS, VOLUME_KEY_LEN};

    const KEY: VolumeKey = [0x5A; VOLUME_KEY_LEN];
    const SECTOR: usize = 512;
    const SECTOR_U32: u32 = 512;

    /// Deterministic test entropy; production uses the platform RNG.
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

    /// A minimal in-memory block device for formatting a test volume.
    struct VecBlock(Vec<u8>);

    impl Block for VecBlock {
        fn geometry(&self) -> Result<tairix_abi::driver::block::BlockGeometry, DriverError> {
            Ok(tairix_abi::driver::block::BlockGeometry {
                block_size: SECTOR_U32,
                block_count: (self.0.len() / SECTOR) as u64,
            })
        }

        fn read_blocks(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DriverError> {
            let start = usize::try_from(lba).map_err(|_| DriverError::OutOfRange)? * SECTOR;
            let end = start + buf.len();
            if end > self.0.len() {
                return Err(DriverError::OutOfRange);
            }
            buf.copy_from_slice(&self.0[start..end]);
            Ok(())
        }

        fn write_blocks(&mut self, lba: u64, buf: &[u8]) -> Result<(), DriverError> {
            let start = usize::try_from(lba).map_err(|_| DriverError::OutOfRange)? * SECTOR;
            let end = start + buf.len();
            if end > self.0.len() {
                return Err(DriverError::OutOfRange);
            }
            self.0[start..end].copy_from_slice(buf);
            Ok(())
        }

        fn flush(&mut self) -> Result<(), DriverError> {
            Ok(())
        }
    }

    /// Format a small volume laid out like the shipped root: the
    /// `/System/Security/{Users,Groups}` files (system-owned) and an empty
    /// `/Users` tree. The driver lock is leaked to `'static`, exactly as
    /// `LateFilesystem::register` leaks the production instance.
    fn backing() -> RootAdminBacking<ARXFS<VecBlock>> {
        let block = VecBlock(alloc::vec![0u8; 4 * 1024 * 1024]);
        let mut fs =
            ARXFS::format(block, 64, &KEY, &mut TestEntropy(3)).expect("test volume formats");
        let root = fs.root();
        let system = fs
            .create(root, b"System", NodeKind::Directory)
            .expect("mkdir System");
        let security = fs
            .create(system, b"Security", NodeKind::Directory)
            .expect("mkdir Security");
        for name in [&b"Users"[..], &b"Groups"[..]] {
            fs.create(security, name, NodeKind::RegularFile)
                .expect("create db file");
            let written = fs
                .write_at(security, name, 0, b"seed")
                .expect("seed db file");
            assert_eq!(written, 4);
        }
        fs.create(root, b"Users", NodeKind::Directory)
            .expect("mkdir Users");
        RootAdminBacking::new(Arc::new(SleepLock::new(fs)))
    }

    fn read_file(backing: &RootAdminBacking<ARXFS<VecBlock>>, name: &[u8]) -> Vec<u8> {
        let mut fs = backing.fs.lock();
        let fs = &mut *fs;
        let security = resolve_security_dir(fs).expect("Security resolves");
        let node = fs.lookup(security, name).expect("file exists");
        let info = fs.node_info(node).expect("info");
        let mut buf = alloc::vec![0u8; usize::try_from(info.size).expect("fits")];
        let read = fs.read_at(node, 0, &mut buf).expect("reads");
        assert_eq!(read, buf.len());
        buf
    }

    #[test]
    fn persist_replaces_both_databases_whole_and_keeps_their_security() {
        let backing = backing();
        let before = {
            let mut fs = backing.fs.lock();
            let fs = &mut *fs;
            let security = resolve_security_dir(fs).expect("resolves");
            let node = fs.lookup(security, b"Users").expect("exists");
            fs.security(node).expect("security")
        };
        backing
            .persist("tairix-users-v1\n", "tairix-groups-v1\n")
            .expect("persists");
        assert_eq!(read_file(&backing, b"Users"), b"tairix-users-v1\n");
        assert_eq!(read_file(&backing, b"Groups"), b"tairix-groups-v1\n");
        // The replacement carries the original's security record, and no
        // temporary is left behind.
        let mut fs = backing.fs.lock();
        let fs = &mut *fs;
        let security_dir = resolve_security_dir(fs).expect("resolves");
        let node = fs.lookup(security_dir, b"Users").expect("exists");
        let after = fs.security(node).expect("security");
        assert_eq!(after.mode, before.mode);
        assert_eq!(after.uid, before.uid);
        assert_eq!(after.gid, before.gid);
        assert_eq!(
            fs.lookup(security_dir, b"Users.tmp").unwrap_err(),
            DriverError::NotFound
        );
    }

    #[test]
    fn persist_fails_closed_when_a_database_is_missing() {
        let backing = backing();
        {
            let mut fs = backing.fs.lock();
            let fs = &mut *fs;
            let security = resolve_security_dir(fs).expect("resolves");
            fs.remove(security, b"Groups").expect("removes");
        }
        assert_eq!(
            backing.persist("tairix-users-v1\n", "tairix-groups-v1\n"),
            Err(Errno::NotFound)
        );
    }

    #[test]
    fn provision_home_creates_an_owner_only_directory_idempotently() {
        let backing = backing();
        backing
            .provision_home("/Users/grace", 1001, 100)
            .expect("provisions");
        {
            let mut fs = backing.fs.lock();
            let fs = &mut *fs;
            let users = fs.lookup(fs.root(), b"Users").expect("Users exists");
            let home = fs.lookup(users, b"grace").expect("home exists");
            let info = fs.node_info(home).expect("info");
            assert_eq!(info.kind, NodeKind::Directory);
            let security = fs.security(home).expect("security");
            assert_eq!(security.mode, HOME_MODE);
            assert_eq!(security.uid, 1001);
            assert_eq!(security.gid, 100);
        }
        // A second provisioning of the same home is a no-op success.
        backing
            .provision_home("/Users/grace", 1001, 100)
            .expect("idempotent");

        // A relative or empty path is refused fail-closed.
        assert_eq!(
            backing.provision_home("Users/grace", 1, 1),
            Err(Errno::OutOfRange)
        );
        assert_eq!(backing.provision_home("/", 1, 1), Err(Errno::OutOfRange));
    }

    #[test]
    fn provision_home_refuses_a_leaf_that_is_a_file() {
        let backing = backing();
        {
            let mut fs = backing.fs.lock();
            let fs = &mut *fs;
            let users = fs.lookup(fs.root(), b"Users").expect("Users exists");
            fs.create(users, b"grace", NodeKind::RegularFile)
                .expect("plant a file");
        }
        assert_eq!(
            backing.provision_home("/Users/grace", 1001, 100),
            Err(Errno::AlreadyExists)
        );
    }
}
