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
//! and creates the directory path, stamps the leaf owner-only under the
//! new account's identity, and fills in the fixed home shape
//! ([`tairix_users::HOME_SUBDIRS`]) inside it; an already-present
//! directory is left exactly as it is (idempotent) — except for the gated
//! per-app data roots and the search-only transit grants that reach them,
//! which are OS shape rather than the account's data and are re-asserted every
//! run so a store is never unreachable.

use alloc::sync::Arc;

use tairix_abi::driver::filesystem::{
    FilesystemRead, FilesystemSecurity, FilesystemWrite, NodeId, NodeKind, NodeSecurity,
};
use tairix_abi::{DriverError, Errno};
use tairix_kernel_core::{SleepLock, UserAdminBacking, VfsError};
use tairix_users::{
    appdata_root_security, appdata_transit_security, APPDATA_ROOT, APPDATA_ROOT_PARENTS, HOME_MODE,
    HOME_SUBDIRS,
};

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
        let mut leaf = None;
        let mut remaining = components.peekable();
        while let Some(component) = remaining.next() {
            if component.is_empty() {
                return Err(Errno::OutOfRange);
            }
            let is_leaf = remaining.peek().is_none();
            match fs.lookup(dir, component.as_bytes()) {
                Ok(node) => {
                    // An already-present directory keeps its ownership:
                    // provisioning is idempotent and never rewrites
                    // existing data. A non-directory in the path is not a
                    // home and is refused rather than replaced.
                    let info = fs.node_info(node).map_err(driver_errno)?;
                    if info.kind != NodeKind::Directory {
                        return Err(Errno::AlreadyExists);
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
                    }
                    dir = node;
                }
                Err(err) => return Err(driver_errno(err)),
            }
            if is_leaf {
                leaf = Some(dir);
            }
        }
        // A path with no components ("/") provisions nothing.
        let Some(home) = leaf else {
            return Err(Errno::OutOfRange);
        };
        // Fill in the shape only inside a home this account owns. An
        // administrator may point a new account at a directory that
        // already exists; creating directories for the new account inside
        // somebody else's home would be provisioning one principal's
        // storage into another's.
        if fs.security(home).map_err(driver_errno)?.uid == uid {
            ensure_home_shape(fs, home, uid, gid)?;
        }
        fs.flush().map_err(driver_errno)
    }
}

/// Create the fixed home shape inside `home`, each directory owned by
/// `(uid, gid)` and owner-only.
///
/// A name already present keeps whatever it holds — the account's own data is
/// never rewritten — and only a missing one is created. Creating them with the
/// account is what lets the first per-user write land: those paths are a level
/// below the home, and the writers create only their immediate parent.
///
/// The [`APPDATA_ROOT_PARENTS`] are the exception the app-data store needs.
/// Their gated root is owned by the app-data service, so nothing running as
/// the account could create it, and the service reaches it only if every
/// directory on the way carries the search-only transit grant. Those records
/// are OS shape rather than the account's data, so they are re-asserted on
/// every provisioning run: a home that merely *exists* is otherwise a home
/// whose store can never be reached.
fn ensure_home_shape<F>(fs: &mut F, home: NodeId, uid: u32, gid: u32) -> Result<(), Errno>
where
    F: FilesystemRead + FilesystemWrite + FilesystemSecurity + ?Sized,
{
    let transit = appdata_transit_security(uid, gid).map_err(driver_errno)?;
    fs.set_security(home, transit).map_err(driver_errno)?;
    for name in HOME_SUBDIRS {
        let node = match fs.lookup(home, name.as_bytes()) {
            Ok(node) => node,
            Err(DriverError::NotFound) => {
                let node = fs
                    .create(home, name.as_bytes(), NodeKind::Directory)
                    .map_err(driver_errno)?;
                fs.set_security(node, NodeSecurity::new(HOME_MODE, uid, gid))
                    .map_err(driver_errno)?;
                node
            }
            Err(err) => return Err(driver_errno(err)),
        };
        if APPDATA_ROOT_PARENTS.contains(&name) {
            fs.set_security(node, transit).map_err(driver_errno)?;
            ensure_appdata_root(fs, node)?;
        }
    }
    Ok(())
}

/// Ensure `parent` holds the gated per-app data root, with the record only the
/// app-data service can reach through.
///
/// A root that is already there is re-stamped rather than trusted: a
/// pre-existing directory of that name could only have come from a principal
/// that is not the service, and the store must not be served out of one.
fn ensure_appdata_root<F>(fs: &mut F, parent: NodeId) -> Result<(), Errno>
where
    F: FilesystemRead + FilesystemWrite + FilesystemSecurity + ?Sized,
{
    let root = match fs.lookup(parent, APPDATA_ROOT.as_bytes()) {
        Ok(node) => {
            if fs.node_info(node).map_err(driver_errno)?.kind != NodeKind::Directory {
                return Err(Errno::AlreadyExists);
            }
            node
        }
        Err(DriverError::NotFound) => fs
            .create(parent, APPDATA_ROOT.as_bytes(), NodeKind::Directory)
            .map_err(driver_errno)?,
        Err(err) => return Err(driver_errno(err)),
    };
    fs.set_security(root, appdata_root_security())
        .map_err(driver_errno)
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
    // The whole database or none of it: a driver may store less than it was
    // handed as back-pressure, so the write resumes rather than being read as
    // a failure — but a genuine refusal still drops the temp instead of
    // letting a truncated database be renamed over the live one.
    if let Err(err) = fs.write_all(dir, tmp_name, 0, data) {
        let _ = fs.remove(dir, tmp_name);
        return Err(driver_errno(err));
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

    /// A provisioned home carries the fixed shape, each directory owned by
    /// the account and owner-only. Without it the first per-user write —
    /// a settings store, an app cache — lands on a missing ancestor.
    #[test]
    fn provision_home_lays_down_the_fixed_home_shape() {
        let backing = backing();
        backing
            .provision_home("/Users/grace", 1001, 100)
            .expect("provisions");

        let mut fs = backing.fs.lock();
        let fs = &mut *fs;
        let users = fs.lookup(fs.root(), b"Users").expect("Users exists");
        let home = fs.lookup(users, b"grace").expect("home exists");
        for name in HOME_SUBDIRS {
            let node = fs
                .lookup(home, name.as_bytes())
                .unwrap_or_else(|_| panic!("{name} exists in a fresh home"));
            assert_eq!(
                fs.node_info(node).expect("info").kind,
                NodeKind::Directory,
                "{name} is a directory"
            );
            let security = fs.security(node).expect("security");
            assert_eq!(security.mode, HOME_MODE, "{name} is owner-only");
            assert_eq!(security.uid, 1001, "{name} belongs to the account");
            assert_eq!(security.gid, 100, "{name} carries the primary group");
        }
    }

    /// The gated per-app data roots are provisioned with the account, owned by
    /// the app-data service, and reachable by it through the search-only
    /// transit grant — and by nothing else.
    #[test]
    fn provision_home_lays_down_the_gated_per_app_data_roots() {
        let backing = backing();
        backing
            .provision_home("/Users/grace", 1001, 100)
            .expect("provisions");

        let mut fs = backing.fs.lock();
        let fs = &mut *fs;
        let users = fs.lookup(fs.root(), b"Users").expect("Users exists");
        let home = fs.lookup(users, b"grace").expect("home exists");
        let transit = appdata_transit_security(1001, 100).expect("one entry fits");
        assert_eq!(fs.security(home).expect("security"), transit);
        for parent in APPDATA_ROOT_PARENTS {
            let node = fs
                .lookup(home, parent.as_bytes())
                .unwrap_or_else(|_| panic!("{parent} exists"));
            assert_eq!(fs.security(node).expect("security"), transit);
            let gated = fs
                .lookup(node, APPDATA_ROOT.as_bytes())
                .unwrap_or_else(|_| panic!("{parent}/{APPDATA_ROOT} exists"));
            assert_eq!(
                fs.security(gated).expect("security"),
                appdata_root_security()
            );
        }
    }

    /// A directory an application planted at the gated root's name is
    /// re-stamped with the service's own record rather than served out of:
    /// only the service could legitimately have created it, so a pre-existing
    /// one is never trusted.
    #[test]
    fn provision_home_reclaims_a_planted_app_data_root() {
        let backing = backing();
        backing
            .provision_home("/Users/grace", 1001, 100)
            .expect("provisions");
        {
            let mut fs = backing.fs.lock();
            let fs = &mut *fs;
            let users = fs.lookup(fs.root(), b"Users").expect("Users exists");
            let home = fs.lookup(users, b"grace").expect("home exists");
            let settings = fs.lookup(home, b"Settings").expect("Settings exists");
            let gated = fs
                .lookup(settings, APPDATA_ROOT.as_bytes())
                .expect("gated root exists");
            // An account-owned, world-writable, ungated decoy.
            fs.set_security(gated, NodeSecurity::new(0o777, 1001, 100))
                .expect("the decoy is planted");
        }

        backing
            .provision_home("/Users/grace", 1001, 100)
            .expect("re-provisions");

        let mut fs = backing.fs.lock();
        let fs = &mut *fs;
        let users = fs.lookup(fs.root(), b"Users").expect("Users exists");
        let home = fs.lookup(users, b"grace").expect("home exists");
        let settings = fs.lookup(home, b"Settings").expect("Settings exists");
        let gated = fs
            .lookup(settings, APPDATA_ROOT.as_bytes())
            .expect("gated root exists");
        assert_eq!(
            fs.security(gated).expect("security"),
            appdata_root_security(),
            "the gate is re-asserted, not inherited"
        );
    }

    /// Re-provisioning fills in a missing directory and leaves an existing
    /// one exactly as it is, including one the account replaced with a file
    /// of its own: provisioning never rewrites a user's own data.
    #[test]
    fn provision_home_repairs_a_missing_shape_without_rewriting_what_is_there() {
        let backing = backing();
        backing
            .provision_home("/Users/grace", 1001, 100)
            .expect("provisions");
        {
            let mut fs = backing.fs.lock();
            let fs = &mut *fs;
            let users = fs.lookup(fs.root(), b"Users").expect("Users exists");
            let home = fs.lookup(users, b"grace").expect("home exists");
            let settings = fs.lookup(home, b"Settings").expect("Settings exists");
            fs.remove(settings, APPDATA_ROOT.as_bytes())
                .expect("removes the gated root");
            fs.remove(home, b"Settings").expect("removes Settings");
            fs.remove(home, b"Desktop").expect("removes Desktop");
            fs.create(home, b"Desktop", NodeKind::RegularFile)
                .expect("the account puts a file there instead");
        }

        backing
            .provision_home("/Users/grace", 1001, 100)
            .expect("idempotent");

        let mut fs = backing.fs.lock();
        let fs = &mut *fs;
        let users = fs.lookup(fs.root(), b"Users").expect("Users exists");
        let home = fs.lookup(users, b"grace").expect("home exists");
        let settings = fs.lookup(home, b"Settings").expect("Settings is restored");
        assert_eq!(
            fs.node_info(settings).expect("info").kind,
            NodeKind::Directory
        );
        let desktop = fs.lookup(home, b"Desktop").expect("Desktop is still there");
        assert_eq!(
            fs.node_info(desktop).expect("info").kind,
            NodeKind::RegularFile,
            "what the account put there is left alone"
        );
    }

    /// An administrator pointing a new account at a directory that already
    /// belongs to somebody else provisions nothing inside it: one
    /// principal's storage is never laid out in another's home.
    #[test]
    fn provision_home_never_lays_a_shape_inside_another_accounts_home() {
        let backing = backing();
        backing
            .provision_home("/Users/grace", 1001, 100)
            .expect("provisions grace");
        {
            let mut fs = backing.fs.lock();
            let fs = &mut *fs;
            let users = fs.lookup(fs.root(), b"Users").expect("Users exists");
            let home = fs.lookup(users, b"grace").expect("home exists");
            let settings = fs.lookup(home, b"Settings").expect("Settings exists");
            fs.remove(settings, APPDATA_ROOT.as_bytes())
                .expect("removes the gated root");
            fs.remove(home, b"Settings").expect("removes Settings");
        }

        backing
            .provision_home("/Users/grace", 2002, 200)
            .expect("succeeds without touching the home");

        let mut fs = backing.fs.lock();
        let fs = &mut *fs;
        let users = fs.lookup(fs.root(), b"Users").expect("Users exists");
        let home = fs.lookup(users, b"grace").expect("home exists");
        assert_eq!(
            fs.lookup(home, b"Settings").unwrap_err(),
            DriverError::NotFound,
            "nothing was created for the other account"
        );
        assert_eq!(
            fs.security(home).expect("security").uid,
            1001,
            "and the owner is unchanged"
        );
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
